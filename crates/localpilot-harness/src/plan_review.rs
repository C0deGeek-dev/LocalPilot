//! What a plan must satisfy before anyone approves it, and how it reads.
//!
//! A plan is syntactically valid long before it is a good plan. `PROGRESS.md`
//! parses whenever its steps are well formed; this module answers the different
//! question of whether a draft is fit to replace the project's plan — every
//! acceptance criterion owned by some step, every step saying how it is
//! verified, and a declared order that cannot point forward.
//!
//! Validation runs against the brief the plan is about to be bound to, not
//! against whatever `brief.md` says later: the criterion numbers a reviewer
//! approved must mean what they meant when the decision was made.

use crate::brief::Brief;
use crate::progress::{Progress, Step, Verification};

/// A reason a draft cannot be approved as it stands.
///
/// Every variant names the step or criterion it is about, because a plan review
/// that says "invalid" is a review nobody can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanDefect {
    /// A step is missing one of the three planning fields. Absence is unknown,
    /// and a plan approved with unknowns is a plan nobody decided.
    MissingMetadata { step: usize, field: &'static str },
    /// A step claims a criterion the bound brief does not have.
    UnknownCriterion {
        step: usize,
        criterion: usize,
        criteria: usize,
    },
    /// A criterion no step owns. The plan would ship without doing it.
    OrphanCriterion { criterion: usize, text: String },
    /// A dependency on a step that does not exist.
    UnknownDependency { step: usize, depends_on: usize },
    /// A dependency on a later step, or on itself. Declared order that points
    /// forward is not an order, and it is how a cycle gets in.
    ForwardDependency { step: usize, depends_on: usize },
}

impl std::fmt::Display for PlanDefect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingMetadata { step, field } => write!(
                f,
                "step {step} does not say '{field}'; an approved plan states all three of covers, verify and depends"
            ),
            Self::UnknownCriterion {
                step,
                criterion,
                criteria,
            } => write!(
                f,
                "step {step} covers AC{criterion}, but the brief has {criteria} acceptance criteria"
            ),
            Self::OrphanCriterion { criterion, text } => write!(
                f,
                "no step covers AC{criterion} ({text}); every acceptance criterion needs an owner"
            ),
            Self::UnknownDependency { step, depends_on } => {
                write!(f, "step {step} depends on step {depends_on}, which does not exist")
            }
            Self::ForwardDependency { step, depends_on } => write!(
                f,
                "step {step} depends on step {depends_on}, which does not come before it"
            ),
        }
    }
}

/// Check a draft against the brief it would be bound to.
///
/// Returns every defect rather than the first: a reviewer fixing one problem at
/// a time, told only about the next one each round, is a reviewer we have wasted.
///
/// # Errors
/// Returns the defects when the draft cannot be approved as it stands.
pub fn validate_for_approval(progress: &Progress, brief: &Brief) -> Result<(), Vec<PlanDefect>> {
    let mut defects = Vec::new();
    let criteria = brief.acceptance_criteria.len();
    let numbers: Vec<usize> = progress.steps.iter().map(|step| step.number).collect();
    let mut owned = vec![false; criteria];

    for step in &progress.steps {
        check_metadata(step, &mut defects);

        if let Some(covers) = &step.covers {
            for criterion in covers {
                if *criterion == 0 || *criterion > criteria {
                    defects.push(PlanDefect::UnknownCriterion {
                        step: step.number,
                        criterion: *criterion,
                        criteria,
                    });
                } else {
                    owned[*criterion - 1] = true;
                }
            }
        }

        if let Some(depends) = &step.depends {
            for dependency in depends {
                if !numbers.contains(dependency) {
                    defects.push(PlanDefect::UnknownDependency {
                        step: step.number,
                        depends_on: *dependency,
                    });
                } else if *dependency >= step.number {
                    // Only earlier steps, so the declared order is a real order
                    // and a cycle cannot be written at all.
                    defects.push(PlanDefect::ForwardDependency {
                        step: step.number,
                        depends_on: *dependency,
                    });
                }
            }
        }
    }

    for (index, is_owned) in owned.iter().enumerate() {
        if !is_owned {
            defects.push(PlanDefect::OrphanCriterion {
                criterion: index + 1,
                text: brief.acceptance_criteria[index].clone(),
            });
        }
    }

    if defects.is_empty() {
        Ok(())
    } else {
        Err(defects)
    }
}

fn check_metadata(step: &Step, defects: &mut Vec<PlanDefect>) {
    // Historical silence must not be replaced with a check that never ran.
    // Persistence separately proves that every completed step is recorded work;
    // only future work needs new planning decisions and criterion ownership.
    if step.done {
        return;
    }
    for (missing, field) in [
        (step.covers.is_none(), "covers"),
        (step.verify.is_none(), "verify"),
        (step.depends.is_none(), "depends"),
    ] {
        if missing {
            defects.push(PlanDefect::MissingMetadata {
                step: step.number,
                field,
            });
        }
    }
}

/// The brief's acceptance criteria, numbered as the plan's `covers` refers to
/// them, so a review shows `AC3` beside the sentence it means rather than a bare
/// label the reader has to resolve for themselves.
#[must_use]
pub fn numbered_criteria(brief: &Brief) -> Vec<(usize, &str)> {
    brief
        .acceptance_criteria
        .iter()
        .enumerate()
        .map(|(index, text)| (index + 1, text.as_str()))
        .collect()
}

/// Which steps own each criterion, for the review surface.
#[must_use]
pub fn coverage(progress: &Progress, brief: &Brief) -> Vec<(usize, String, Vec<usize>)> {
    numbered_criteria(brief)
        .into_iter()
        .map(|(number, text)| {
            let owners = progress
                .steps
                .iter()
                .filter(|step| {
                    step.covers
                        .as_ref()
                        .is_some_and(|covers| covers.contains(&number))
                })
                .map(|step| step.number)
                .collect();
            (number, text.to_string(), owners)
        })
        .collect()
}

/// How a step says it is verified, rendered for review.
#[must_use]
pub fn verification_text(step: &Step) -> String {
    match &step.verify {
        Some(Verification::Command(command)) => command.clone(),
        Some(Verification::None { reason }) => format!("nothing to run - {reason}"),
        None if step.done => {
            "historical verification not recorded; this step will not run again".to_string()
        }
        None => "not stated".to_string(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const BRIEF: &str = "# Brief: thing\n\n## Summary\n\nDo it.\n\n## Requirements\n\n\
- It works\n\n## Constraints\n\n- Be small\n\n## Non-Goals\n\n- Else\n\n\
## Acceptance Criteria\n\n- The parser accepts a valid file\n- The gate refuses a bad one\n";

    fn brief() -> Brief {
        Brief::parse(BRIEF).unwrap()
    }

    fn plan(steps: &str) -> Progress {
        Progress::parse(&format!(
            "# Progress: thing\nBranch: feature/thing\n\n## Steps\n\n{steps}"
        ))
        .unwrap()
    }

    #[test]
    fn a_complete_plan_passes() {
        let progress = plan(
            "- [ ] 1. Write the parser\n  - covers: AC1\n  - verify: cargo test parser\n  \
- depends: none\n- [ ] 2. Write the gate\n  - covers: AC2\n  - verify: cargo test gate\n  \
- depends: 1\n",
        );
        validate_for_approval(&progress, &brief()).unwrap();
    }

    #[test]
    fn a_legacy_plan_is_not_approvable_and_says_which_fields_are_missing() {
        // Absence is unknown. Approving a plan with unknowns would record a
        // decision nobody made.
        let progress = plan("- [ ] 1. Write the parser\n- [ ] 2. Write the gate\n");
        let defects = validate_for_approval(&progress, &brief()).unwrap_err();
        for field in ["covers", "verify", "depends"] {
            assert!(
                defects.contains(&PlanDefect::MissingMetadata { step: 1, field }),
                "{field} missing from {defects:?}"
            );
        }
    }

    #[test]
    fn every_criterion_needs_an_owner() {
        let progress = plan(
            "- [ ] 1. Write the parser\n  - covers: AC1\n  - verify: cargo test parser\n  \
- depends: none\n",
        );
        let defects = validate_for_approval(&progress, &brief()).unwrap_err();
        assert!(defects.iter().any(|defect| matches!(
            defect,
            PlanDefect::OrphanCriterion { criterion: 2, text } if text == "The gate refuses a bad one"
        )), "{defects:?}");
    }

    #[test]
    fn a_step_cannot_own_a_criterion_the_brief_does_not_have() {
        // The mirror of an orphan: a plan claiming a requirement nobody asked
        // for is as wrong as one that drops a requirement.
        let progress = plan(
            "- [ ] 1. Everything\n  - covers: AC1, AC2, AC9\n  - verify: cargo test\n  \
- depends: none\n",
        );
        let defects = validate_for_approval(&progress, &brief()).unwrap_err();
        assert!(
            defects.contains(&PlanDefect::UnknownCriterion {
                step: 1,
                criterion: 9,
                criteria: 2
            }),
            "{defects:?}"
        );
    }

    #[test]
    fn declared_order_may_only_point_backwards() {
        let progress = plan(
            "- [ ] 1. First\n  - covers: AC1\n  - verify: cargo test\n  - depends: 2\n\
- [ ] 2. Second\n  - covers: AC2\n  - verify: cargo test\n  - depends: 7\n",
        );
        let defects = validate_for_approval(&progress, &brief()).unwrap_err();
        assert!(
            defects.contains(&PlanDefect::ForwardDependency {
                step: 1,
                depends_on: 2
            }),
            "{defects:?}"
        );
        assert!(
            defects.contains(&PlanDefect::UnknownDependency {
                step: 2,
                depends_on: 7
            }),
            "{defects:?}"
        );
    }

    #[test]
    fn every_defect_is_reported_rather_than_the_first() {
        // A reviewer told one problem per round is a reviewer we have wasted.
        let progress = plan("- [ ] 1. Everything\n  - covers: AC9\n");
        let defects = validate_for_approval(&progress, &brief()).unwrap_err();
        assert!(defects.len() >= 4, "{defects:?}");
    }

    #[test]
    fn the_review_surface_resolves_criterion_numbers_to_their_text() {
        let progress = plan(
            "- [ ] 1. Write the parser\n  - covers: AC1\n  - verify: cargo test parser\n  \
- depends: none\n- [ ] 2. Write the gate\n  - covers: AC2\n  \
- verify: none - covered by the parser test\n  - depends: 1\n",
        );
        let coverage = coverage(&progress, &brief());
        assert_eq!(
            coverage[0],
            (1, "The parser accepts a valid file".to_string(), vec![1])
        );
        assert_eq!(
            coverage[1],
            (2, "The gate refuses a bad one".to_string(), vec![2])
        );
        assert_eq!(
            verification_text(&progress.steps[1]),
            "nothing to run - covered by the parser test"
        );
    }
}
