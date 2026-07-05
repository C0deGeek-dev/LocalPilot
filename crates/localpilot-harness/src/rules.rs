//! The deterministic harness rule engine.
//!
//! Rules layer on top of the permission engine — they can stop or warn about a
//! step, but they never grant a side effect the permission engine would deny.
//! Configuration can tighten a rule's severity but cannot silently disable a
//! critical rule.

use indexmap::IndexMap;
use localpilot_config::redact::contains_secret;
use localpilot_config::{Cadence, RuleSeverity};

use crate::quality::{CheckOutcome, CheckSeverity, CheckStatus};

/// When a rule runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Trigger {
    SessionStart,
    PreTool,
    PostTool,
    PostEdit,
    PreShell,
    PostShell,
    PreCommit,
    PostTest,
    StepComplete,
    PhaseComplete,
}

/// A rule's decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleVerdict {
    /// Continue.
    Allow,
    /// Continue and surface a message.
    Warn(String),
    /// Send the reason back to the model and retry the same step.
    Retry(String),
    /// Reset the working tree for this step and restart with fresh context.
    Discard(String),
    /// Stop and ask the user.
    Block(String),
}

impl RuleVerdict {
    /// Whether the verdict stops progress (block) or merely warns/continues.
    #[must_use]
    pub fn is_blocking(&self) -> bool {
        matches!(self, RuleVerdict::Block(_))
    }

    /// The attached message, if any.
    #[must_use]
    pub fn message(&self) -> Option<&str> {
        match self {
            RuleVerdict::Allow => None,
            RuleVerdict::Warn(m)
            | RuleVerdict::Retry(m)
            | RuleVerdict::Discard(m)
            | RuleVerdict::Block(m) => Some(m),
        }
    }
}

/// Inputs a rule may inspect. Unset fields mean "not applicable to this trigger".
#[derive(Debug, Default, Clone)]
pub struct RuleContext {
    pub uncommitted_unrelated: bool,
    pub commit_message: Option<String>,
    pub tests_passed: Option<bool>,
    pub progress_reflects_completion: Option<bool>,
    pub attempts: u32,
    pub max_attempts: u32,
    /// Outcomes of the quality-gate checks that ran for this trigger, consumed by
    /// the `quality_gate` rule.
    pub gate_outcomes: Vec<CheckOutcome>,
    /// A local serveable target was named in the task prompt and has not been
    /// probed this session (computed from the evidence ledger). Consumed by the
    /// `check_before_launch` rule.
    pub named_local_target_unprobed: bool,
    /// The action being gated starts a local HTTP server or scaffolds a competing
    /// entry file. Consumed by the `check_before_launch` rule.
    pub launch_or_scaffold_attempt: bool,
}

/// A harness rule.
pub trait Rule: Send + Sync {
    /// The rule's stable name (matches a `[harness.rules]` key).
    fn name(&self) -> &'static str;
    /// Whether the rule runs on the given trigger.
    fn applies_to(&self, trigger: Trigger) -> bool;
    /// The out-of-box severity.
    fn default_severity(&self) -> RuleSeverity;
    /// Whether configuration may disable the rule. Critical rules may be made
    /// stricter but never turned `Off`.
    fn critical(&self) -> bool {
        false
    }
    /// Evaluate the rule at the given (already-clamped) severity.
    fn evaluate(&self, ctx: &RuleContext, severity: RuleSeverity) -> RuleVerdict;
}

/// Map a violation to a verdict at a severity. `Off` disables (allow).
fn at(severity: RuleSeverity, reason: impl Into<String>) -> RuleVerdict {
    match severity {
        RuleSeverity::Off => RuleVerdict::Allow,
        RuleSeverity::Warn => RuleVerdict::Warn(reason.into()),
        RuleSeverity::Block => RuleVerdict::Block(reason.into()),
    }
}

macro_rules! rule {
    ($ty:ident, $name:literal, critical = $crit:expr, default = $sev:expr, triggers = [$($t:ident),*]) => {
        /// Baseline rule.
        pub struct $ty;
        impl $ty {
            fn fires(&self, trigger: Trigger) -> bool {
                matches!(trigger, $(Trigger::$t)|*)
            }
        }
        impl Rule for $ty {
            fn name(&self) -> &'static str { $name }
            fn applies_to(&self, trigger: Trigger) -> bool { self.fires(trigger) }
            fn default_severity(&self) -> RuleSeverity { $sev }
            fn critical(&self) -> bool { $crit }
            fn evaluate(&self, ctx: &RuleContext, severity: RuleSeverity) -> RuleVerdict {
                $ty::check(self, ctx, severity)
            }
        }
    };
}

rule!(
    NoStaleUncommitted,
    "no_stale_uncommitted",
    critical = false,
    default = RuleSeverity::Block,
    triggers = [SessionStart]
);
rule!(
    SuiteGreen,
    "suite_green",
    critical = true,
    default = RuleSeverity::Block,
    triggers = [PostTest, StepComplete]
);
rule!(
    ProgressUpdated,
    "progress_updated",
    critical = false,
    // Advisory by default: the harness itself ticks PROGRESS.md when it commits a
    // step (`Progress::mark_complete` on the commit path), so a not-yet-ticked
    // step is a flag, not a hard stop — a Block here would deadlock the harness's
    // own commit-and-tick flow. Configure to `block` to require the model to tick.
    default = RuleSeverity::Warn,
    triggers = [PreCommit, StepComplete]
);
rule!(
    CommitMessageClean,
    "commit_message_clean",
    critical = true,
    default = RuleSeverity::Block,
    triggers = [PreCommit]
);
rule!(
    AttemptLimit,
    "attempt_limit",
    critical = false,
    default = RuleSeverity::Block,
    triggers = [StepComplete]
);
rule!(
    QualityGate,
    "quality_gate",
    critical = true,
    default = RuleSeverity::Block,
    triggers = [StepComplete, PhaseComplete]
);
rule!(
    CheckBeforeLaunch,
    "check_before_launch",
    critical = false,
    default = RuleSeverity::Warn,
    triggers = [PreShell, PreTool]
);

const PROHIBITED_COMMIT_TERMS: &[&str] = &["leaked", "source-map", "private endpoint"];

impl NoStaleUncommitted {
    fn check(&self, ctx: &RuleContext, severity: RuleSeverity) -> RuleVerdict {
        if ctx.uncommitted_unrelated {
            at(
                severity,
                "unrelated uncommitted changes are present; commit or stash them first",
            )
        } else {
            RuleVerdict::Allow
        }
    }
}

impl SuiteGreen {
    fn check(&self, ctx: &RuleContext, severity: RuleSeverity) -> RuleVerdict {
        if ctx.tests_passed == Some(false) {
            at(severity, "the configured test command did not pass")
        } else {
            RuleVerdict::Allow
        }
    }
}

impl ProgressUpdated {
    fn check(&self, ctx: &RuleContext, severity: RuleSeverity) -> RuleVerdict {
        if ctx.progress_reflects_completion == Some(false) {
            at(
                severity,
                "PROGRESS.md does not yet reflect the completed step",
            )
        } else {
            RuleVerdict::Allow
        }
    }
}

impl CommitMessageClean {
    fn check(&self, ctx: &RuleContext, severity: RuleSeverity) -> RuleVerdict {
        let Some(message) = &ctx.commit_message else {
            return RuleVerdict::Allow;
        };
        let lower = message.to_ascii_lowercase();
        if contains_secret(message) || PROHIBITED_COMMIT_TERMS.iter().any(|t| lower.contains(t)) {
            at(
                severity,
                "the commit message contains a secret or a prohibited reference",
            )
        } else {
            RuleVerdict::Allow
        }
    }
}

impl AttemptLimit {
    fn check(&self, ctx: &RuleContext, severity: RuleSeverity) -> RuleVerdict {
        if ctx.max_attempts > 0 && ctx.attempts >= ctx.max_attempts {
            at(severity, "the per-step attempt limit was reached")
        } else {
            RuleVerdict::Allow
        }
    }
}

impl QualityGate {
    fn check(&self, ctx: &RuleContext, severity: RuleSeverity) -> RuleVerdict {
        gate_verdict(&ctx.gate_outcomes, severity)
    }
}

impl CheckBeforeLaunch {
    fn check(&self, ctx: &RuleContext, severity: RuleSeverity) -> RuleVerdict {
        if ctx.named_local_target_unprobed && ctx.launch_or_scaffold_attempt {
            at(
                severity,
                "a target URL or host was named in the task but has not been probed this session; \
                 probe it first (e.g. fetch or curl it) and only launch your own server if the \
                 probe fails",
            )
        } else {
            RuleVerdict::Allow
        }
    }
}

/// Reduce quality-gate outcomes to one verdict. A passing check contributes
/// `Allow`; a denied or un-runnable check blocks; a failing check maps by its
/// configured severity — explicit `block` (e.g. `audit`) blocks, `warn` warns,
/// `off` is ignored, and the default (no override) is `retry`, the actionable
/// path the loop feeds back to the model. The rule's own severity is a ceiling:
/// `warn` softens everything to a warning, `off` disables the gate.
fn gate_verdict(outcomes: &[CheckOutcome], rule_severity: RuleSeverity) -> RuleVerdict {
    if rule_severity == RuleSeverity::Off {
        return RuleVerdict::Allow;
    }
    let mut worst = RuleVerdict::Allow;
    for outcome in outcomes {
        let verdict = outcome_verdict(outcome);
        if rank(&verdict) > rank(&worst) {
            worst = verdict;
        }
    }
    apply_ceiling(worst, rule_severity)
}

fn outcome_verdict(outcome: &CheckOutcome) -> RuleVerdict {
    let name = &outcome.name;
    match outcome.status {
        CheckStatus::Passed => RuleVerdict::Allow,
        CheckStatus::Denied => RuleVerdict::Block(format!(
            "quality check `{name}` was denied by the permission engine"
        )),
        CheckStatus::Errored => RuleVerdict::Block(format!("quality check `{name}` could not run")),
        CheckStatus::Failed => match outcome.severity {
            Some(CheckSeverity::Off) => RuleVerdict::Allow,
            Some(CheckSeverity::Warn) => {
                RuleVerdict::Warn(format!("quality check `{name}` reported findings"))
            }
            Some(CheckSeverity::Block) => {
                RuleVerdict::Block(format!("quality check `{name}` reported blocking findings"))
            }
            None => RuleVerdict::Retry(format!(
                "quality check `{name}` failed; fix the findings and retry"
            )),
        },
    }
}

/// Severity ordering for reducing many outcomes to the most severe verdict.
fn rank(verdict: &RuleVerdict) -> u8 {
    match verdict {
        RuleVerdict::Allow => 0,
        RuleVerdict::Warn(_) => 1,
        RuleVerdict::Retry(_) => 2,
        RuleVerdict::Discard(_) => 3,
        RuleVerdict::Block(_) => 4,
    }
}

/// Apply the rule-level severity as a ceiling on the reduced verdict.
fn apply_ceiling(verdict: RuleVerdict, rule_severity: RuleSeverity) -> RuleVerdict {
    match rule_severity {
        RuleSeverity::Block => verdict,
        RuleSeverity::Off => RuleVerdict::Allow,
        RuleSeverity::Warn => match verdict {
            RuleVerdict::Allow => RuleVerdict::Allow,
            other => RuleVerdict::Warn(
                other
                    .message()
                    .unwrap_or("the quality gate reported findings")
                    .to_string(),
            ),
        },
    }
}

/// The trigger a check of `cadence` evaluates on: step checks at step completion,
/// phase checks at a phase boundary.
#[must_use]
pub fn trigger_for_cadence(cadence: Cadence) -> Trigger {
    match cadence {
        Cadence::Step => Trigger::StepComplete,
        Cadence::Phase => Trigger::PhaseComplete,
    }
}

/// The rule engine: the baseline rules plus configured severities.
pub struct RuleEngine {
    rules: Vec<Box<dyn Rule>>,
    severities: IndexMap<String, RuleSeverity>,
}

impl RuleEngine {
    /// Build the engine with the baseline rules and config-provided severities.
    #[must_use]
    pub fn with_baseline(config: &IndexMap<String, RuleSeverity>) -> Self {
        let rules: Vec<Box<dyn Rule>> = vec![
            Box::new(NoStaleUncommitted),
            Box::new(SuiteGreen),
            Box::new(ProgressUpdated),
            Box::new(CommitMessageClean),
            Box::new(AttemptLimit),
            Box::new(QualityGate),
            Box::new(CheckBeforeLaunch),
        ];
        Self {
            rules,
            severities: config.clone(),
        }
    }

    /// The severity in effect for a rule: the configured value if present, else
    /// the default — but a critical rule is never allowed to be `Off`.
    #[must_use]
    pub fn effective_severity(&self, rule: &dyn Rule) -> RuleSeverity {
        let configured = self
            .severities
            .get(rule.name())
            .copied()
            .unwrap_or_else(|| rule.default_severity());
        if rule.critical() && configured == RuleSeverity::Off {
            // A critical rule cannot be silently disabled; fall back to its
            // (non-Off) default.
            rule.default_severity()
        } else {
            configured
        }
    }

    /// Evaluate every rule for `trigger`, returning the non-allow verdicts.
    #[must_use]
    pub fn evaluate(
        &self,
        trigger: Trigger,
        ctx: &RuleContext,
    ) -> Vec<(&'static str, RuleVerdict)> {
        let mut outcomes = Vec::new();
        for rule in &self.rules {
            if !rule.applies_to(trigger) {
                continue;
            }
            let severity = self.effective_severity(rule.as_ref());
            let verdict = rule.evaluate(ctx, severity);
            if verdict != RuleVerdict::Allow {
                outcomes.push((rule.name(), verdict));
            }
        }
        outcomes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(overrides: &[(&str, RuleSeverity)]) -> RuleEngine {
        let mut map = IndexMap::new();
        for (name, sev) in overrides {
            map.insert((*name).to_string(), *sev);
        }
        RuleEngine::with_baseline(&map)
    }

    #[test]
    fn no_stale_uncommitted_blocks_at_session_start() {
        let ctx = RuleContext {
            uncommitted_unrelated: true,
            ..RuleContext::default()
        };
        let outcomes = engine(&[]).evaluate(Trigger::SessionStart, &ctx);
        assert!(outcomes
            .iter()
            .any(|(n, v)| *n == "no_stale_uncommitted" && v.is_blocking()));
    }

    #[test]
    fn each_baseline_rule_fires_on_its_condition() {
        // suite_green
        assert!(matches!(
            SuiteGreen.evaluate(
                &RuleContext {
                    tests_passed: Some(false),
                    ..Default::default()
                },
                RuleSeverity::Block
            ),
            RuleVerdict::Block(_)
        ));
        // progress_updated
        assert!(matches!(
            ProgressUpdated.evaluate(
                &RuleContext {
                    progress_reflects_completion: Some(false),
                    ..Default::default()
                },
                RuleSeverity::Block
            ),
            RuleVerdict::Block(_)
        ));
        // commit_message_clean
        assert!(matches!(
            CommitMessageClean.evaluate(
                &RuleContext {
                    commit_message: Some("add key sk-abcdefghijklmnopqrstuvwxyz0123".into()),
                    ..Default::default()
                },
                RuleSeverity::Block
            ),
            RuleVerdict::Block(_)
        ));
        // attempt_limit
        assert!(matches!(
            AttemptLimit.evaluate(
                &RuleContext {
                    attempts: 3,
                    max_attempts: 3,
                    ..Default::default()
                },
                RuleSeverity::Block
            ),
            RuleVerdict::Block(_)
        ));
    }

    #[test]
    fn config_can_downgrade_a_non_critical_rule() {
        let ctx = RuleContext {
            uncommitted_unrelated: true,
            ..RuleContext::default()
        };
        let outcomes = engine(&[("no_stale_uncommitted", RuleSeverity::Warn)])
            .evaluate(Trigger::SessionStart, &ctx);
        assert!(outcomes
            .iter()
            .any(|(n, v)| *n == "no_stale_uncommitted" && matches!(v, RuleVerdict::Warn(_))));
    }

    #[test]
    fn config_cannot_downgrade_a_critical_rule_to_allow() {
        let engine = engine(&[("suite_green", RuleSeverity::Off)]);
        // The Off is clamped back to the default (Block).
        assert_eq!(engine.effective_severity(&SuiteGreen), RuleSeverity::Block);
        let ctx = RuleContext {
            tests_passed: Some(false),
            ..RuleContext::default()
        };
        let outcomes = engine.evaluate(Trigger::StepComplete, &ctx);
        assert!(outcomes
            .iter()
            .any(|(n, v)| *n == "suite_green" && v.is_blocking()));
    }

    fn gate_outcome(
        name: &str,
        status: CheckStatus,
        severity: Option<CheckSeverity>,
    ) -> CheckOutcome {
        CheckOutcome {
            name: name.to_string(),
            status,
            detail: String::new(),
            fixed: false,
            severity,
        }
    }

    #[test]
    fn quality_gate_allows_when_all_checks_pass() {
        assert_eq!(
            gate_verdict(
                &[gate_outcome("fmt", CheckStatus::Passed, None)],
                RuleSeverity::Block
            ),
            RuleVerdict::Allow
        );
    }

    #[test]
    fn quality_gate_retries_a_failed_actionable_check() {
        let verdict = gate_verdict(
            &[gate_outcome("clippy", CheckStatus::Failed, None)],
            RuleSeverity::Block,
        );
        assert!(matches!(verdict, RuleVerdict::Retry(_)));
    }

    #[test]
    fn quality_gate_blocks_failed_block_denied_and_errored() {
        for outcome in [
            gate_outcome("audit", CheckStatus::Failed, Some(CheckSeverity::Block)),
            gate_outcome("fmt", CheckStatus::Denied, None),
            gate_outcome("test", CheckStatus::Errored, None),
        ] {
            assert!(gate_verdict(&[outcome], RuleSeverity::Block).is_blocking());
        }
    }

    #[test]
    fn quality_gate_takes_the_most_severe_outcome() {
        let verdict = gate_verdict(
            &[
                gate_outcome("clippy", CheckStatus::Failed, None),
                gate_outcome("audit", CheckStatus::Failed, Some(CheckSeverity::Block)),
            ],
            RuleSeverity::Block,
        );
        assert!(verdict.is_blocking());
    }

    #[test]
    fn quality_gate_warn_ceiling_softens_failures() {
        let verdict = gate_verdict(
            &[gate_outcome("clippy", CheckStatus::Failed, None)],
            RuleSeverity::Warn,
        );
        assert!(matches!(verdict, RuleVerdict::Warn(_)));
    }

    #[test]
    fn quality_gate_is_critical_and_cannot_be_disabled() {
        let engine = engine(&[("quality_gate", RuleSeverity::Off)]);
        assert_eq!(engine.effective_severity(&QualityGate), RuleSeverity::Block);
        let ctx = RuleContext {
            gate_outcomes: vec![gate_outcome(
                "audit",
                CheckStatus::Failed,
                Some(CheckSeverity::Block),
            )],
            ..RuleContext::default()
        };
        let outcomes = engine.evaluate(Trigger::PhaseComplete, &ctx);
        assert!(outcomes
            .iter()
            .any(|(n, v)| *n == "quality_gate" && v.is_blocking()));
    }

    #[test]
    fn quality_gate_fires_on_step_and_phase_only() {
        assert!(QualityGate.applies_to(Trigger::StepComplete));
        assert!(QualityGate.applies_to(Trigger::PhaseComplete));
        assert!(!QualityGate.applies_to(Trigger::PreCommit));
    }

    #[test]
    fn cadence_maps_to_its_trigger() {
        assert_eq!(trigger_for_cadence(Cadence::Step), Trigger::StepComplete);
        assert_eq!(trigger_for_cadence(Cadence::Phase), Trigger::PhaseComplete);
    }

    fn launch_ctx(named_unprobed: bool, attempting: bool) -> RuleContext {
        RuleContext {
            named_local_target_unprobed: named_unprobed,
            launch_or_scaffold_attempt: attempting,
            ..RuleContext::default()
        }
    }

    #[test]
    fn check_before_launch_warns_on_an_unprobed_named_target_launch() {
        let verdict = CheckBeforeLaunch.evaluate(&launch_ctx(true, true), RuleSeverity::Warn);
        match verdict {
            RuleVerdict::Warn(reason) => assert!(reason.contains("probe it first")),
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn check_before_launch_allows_once_the_target_is_probed() {
        // A satisfied probe clears the unprobed signal, exactly like a prior read
        // clears RequiresPriorRead.
        assert_eq!(
            CheckBeforeLaunch.evaluate(&launch_ctx(false, true), RuleSeverity::Warn),
            RuleVerdict::Allow
        );
    }

    #[test]
    fn check_before_launch_allows_when_no_target_named_or_no_launch() {
        assert_eq!(
            CheckBeforeLaunch.evaluate(&launch_ctx(false, false), RuleSeverity::Warn),
            RuleVerdict::Allow
        );
        // A named-but-unprobed target with no launch/scaffold action does not fire.
        assert_eq!(
            CheckBeforeLaunch.evaluate(&launch_ctx(true, false), RuleSeverity::Warn),
            RuleVerdict::Allow
        );
    }

    #[test]
    fn check_before_launch_fires_on_pre_shell_and_pre_tool_only() {
        assert!(CheckBeforeLaunch.applies_to(Trigger::PreShell));
        assert!(CheckBeforeLaunch.applies_to(Trigger::PreTool));
        assert!(!CheckBeforeLaunch.applies_to(Trigger::PreCommit));
        assert!(!CheckBeforeLaunch.applies_to(Trigger::StepComplete));
    }

    #[test]
    fn check_before_launch_defaults_to_warn_and_respects_overrides() {
        // Absent config: the rule's own default (Warn) is in effect.
        let default_engine = engine(&[]);
        assert_eq!(
            default_engine.effective_severity(&CheckBeforeLaunch),
            RuleSeverity::Warn
        );
        let ctx = launch_ctx(true, true);
        assert!(default_engine
            .evaluate(Trigger::PreShell, &ctx)
            .iter()
            .any(|(n, v)| *n == "check_before_launch" && matches!(v, RuleVerdict::Warn(_))));

        // `block` tightens it to a hard stop; `off` disables it (non-critical).
        let blocking = engine(&[("check_before_launch", RuleSeverity::Block)]);
        assert!(blocking
            .evaluate(Trigger::PreShell, &ctx)
            .iter()
            .any(|(n, v)| *n == "check_before_launch" && v.is_blocking()));
        let disabled = engine(&[("check_before_launch", RuleSeverity::Off)]);
        assert!(disabled.evaluate(Trigger::PreShell, &ctx).is_empty());
    }

    #[test]
    fn check_before_launch_is_tighten_only() {
        // The rule can only Allow, Warn, or Block — it never returns a verdict that
        // grants an action, so it cannot turn a denied launch into an allowed one.
        for (named, attempting, severity) in [
            (true, true, RuleSeverity::Warn),
            (true, true, RuleSeverity::Block),
            (false, true, RuleSeverity::Block),
            (true, false, RuleSeverity::Block),
        ] {
            let verdict = CheckBeforeLaunch.evaluate(&launch_ctx(named, attempting), severity);
            assert!(
                matches!(
                    verdict,
                    RuleVerdict::Allow | RuleVerdict::Warn(_) | RuleVerdict::Block(_)
                ),
                "rule produced a non-tightening verdict: {verdict:?}"
            );
        }
    }
}
