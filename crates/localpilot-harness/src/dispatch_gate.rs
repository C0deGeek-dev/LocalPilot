//! The pre-execution gate chain for a single tool call, composed into one
//! ordered decision.
//!
//! Before a tool runs, the session consults several tighten-only mechanisms in a
//! fixed precedence: a broker re-resolution (reveal-never-grant) can redirect the
//! call, a contract precondition can refuse it, and the `check_before_launch`
//! discipline rule can refuse (`Block`) or merely nudge (`Warn`) it. The ordering
//! is a safety property — a refusal must short-circuit before any advisory `Warn`
//! is surfaced, so a refused call never also emits a launch nudge — so it lives in
//! one named, tested place rather than as an implicit `if/else if` chain at the
//! dispatch site. This function is **pure**: it ranks already-computed gate
//! outcomes; it never runs the tool or touches the permission engine (the permission
//! engine itself runs inside `Proceed`, after this decision).

use crate::rules::RuleVerdict;
use localpilot_tools::Resolution;

/// The composed pre-execution decision for one tool call, in precedence order.
#[derive(Debug)]
pub(crate) enum PreDispatch {
    /// The broker re-resolved an unadvertised call to the closest available tool.
    /// The attempted call never runs; the resolution message is surfaced in its
    /// place (reveal-never-grant).
    Redirect(Resolution),
    /// Refuse the call before execution with a model-visible error. `announce`
    /// preserves the original surfacing: a precondition block is quiet (error
    /// result only), a rule `Block` also emits a warning event.
    Block { reason: String, announce: bool },
    /// Run the call through the permission-gated registry. `warn` carries an
    /// advisory `check_before_launch` nudge to append to the result afterwards.
    Proceed { warn: Option<String> },
}

/// Rank the pre-execution gate outcomes into one decision.
///
/// Precedence, highest first: broker redirect → precondition block → rule block →
/// proceed. A `Block` (refusal) wins over a rule `Warn`, so a refused call never
/// also fires the advisory nudge. Only `RuleVerdict::Block`/`Warn` affect the
/// launch decision; any other verdict (or `None`) proceeds quietly, exactly as the
/// original dispatch site did.
pub(crate) fn pre_dispatch_decision(
    reresolution: Option<Resolution>,
    precondition: Option<String>,
    launch_verdict: Option<RuleVerdict>,
) -> PreDispatch {
    if let Some(resolution) = reresolution {
        return PreDispatch::Redirect(resolution);
    }
    if let Some(reason) = precondition {
        return PreDispatch::Block {
            reason,
            announce: false,
        };
    }
    match launch_verdict {
        Some(RuleVerdict::Block(reason)) => PreDispatch::Block {
            reason,
            announce: true,
        },
        Some(RuleVerdict::Warn(message)) => PreDispatch::Proceed {
            warn: Some(message),
        },
        _ => PreDispatch::Proceed { warn: None },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_precondition_block_short_circuits_before_an_advisory_warn() {
        // The precise safety property: a refusal wins over a launch Warn, so the
        // refused call never also surfaces the advisory nudge. A precondition block
        // is the quiet refusal.
        let decision = pre_dispatch_decision(
            None,
            Some("requires a prior read".to_string()),
            Some(RuleVerdict::Warn("probe the target first".to_string())),
        );
        match decision {
            PreDispatch::Block { reason, announce } => {
                assert_eq!(reason, "requires a prior read");
                assert!(!announce, "a precondition block is quiet");
            }
            other => panic!("expected a quiet Block, got {other:?}"),
        }
    }

    #[test]
    fn a_rule_block_refuses_and_is_announced() {
        let decision = pre_dispatch_decision(
            None,
            None,
            Some(RuleVerdict::Block(
                "launch refused: probe first".to_string(),
            )),
        );
        match decision {
            PreDispatch::Block { announce, .. } => assert!(announce, "a rule block announces"),
            other => panic!("expected an announced Block, got {other:?}"),
        }
    }

    #[test]
    fn a_lone_warn_proceeds_carrying_the_nudge() {
        let decision = pre_dispatch_decision(
            None,
            None,
            Some(RuleVerdict::Warn("probe first".to_string())),
        );
        assert!(matches!(decision, PreDispatch::Proceed { warn: Some(m) } if m == "probe first"));
    }

    #[test]
    fn no_objection_proceeds_cleanly() {
        assert!(matches!(
            pre_dispatch_decision(None, None, None),
            PreDispatch::Proceed { warn: None }
        ));
        // A non-launch verdict (Allow/Retry/Discard) also proceeds quietly, exactly
        // as the original dispatch site ignored everything but Block/Warn.
        assert!(matches!(
            pre_dispatch_decision(None, None, Some(RuleVerdict::Allow)),
            PreDispatch::Proceed { warn: None }
        ));
    }
}
