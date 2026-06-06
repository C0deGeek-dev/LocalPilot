//! The deterministic harness rule engine.
//!
//! Rules layer on top of the permission engine — they can stop or warn about a
//! step, but they never grant a side effect the permission engine would deny.
//! Configuration can tighten a rule's severity but cannot silently disable a
//! critical rule.

use indexmap::IndexMap;
use localpilot_config::redact::contains_secret;
use localpilot_config::{Cadence, RuleSeverity};

use crate::quality::{CheckOutcome, CheckStatus};

/// When a rule runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Trigger {
    SessionStart,
    PreTool,
    PostTool,
    PreEdit,
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
pub enum Verdict {
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

impl Verdict {
    /// Whether the verdict stops progress (block) or merely warns/continues.
    #[must_use]
    pub fn is_blocking(&self) -> bool {
        matches!(self, Verdict::Block(_))
    }

    /// The attached message, if any.
    #[must_use]
    pub fn message(&self) -> Option<&str> {
        match self {
            Verdict::Allow => None,
            Verdict::Warn(m) | Verdict::Retry(m) | Verdict::Discard(m) | Verdict::Block(m) => {
                Some(m)
            }
        }
    }
}

/// Inputs a rule may inspect. Unset fields mean "not applicable to this trigger".
#[derive(Debug, Default, Clone)]
pub struct RuleContext {
    pub uncommitted_unrelated: bool,
    pub path_inside_workspace: Option<bool>,
    pub path_secret_like: bool,
    pub commit_message: Option<String>,
    pub tests_passed: Option<bool>,
    pub progress_reflects_completion: Option<bool>,
    pub test_first_required: bool,
    pub editing_impl_before_tests: bool,
    pub attempts: u32,
    pub max_attempts: u32,
    /// Outcomes of the quality-gate checks that ran for this trigger, consumed by
    /// the `quality_gate` rule.
    pub gate_outcomes: Vec<CheckOutcome>,
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
    fn evaluate(&self, ctx: &RuleContext, severity: RuleSeverity) -> Verdict;
}

/// Map a violation to a verdict at a severity. `Off` disables (allow).
fn at(severity: RuleSeverity, reason: impl Into<String>) -> Verdict {
    match severity {
        RuleSeverity::Off => Verdict::Allow,
        RuleSeverity::Warn => Verdict::Warn(reason.into()),
        RuleSeverity::Block => Verdict::Block(reason.into()),
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
            fn evaluate(&self, ctx: &RuleContext, severity: RuleSeverity) -> Verdict {
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
    WorkspaceBoundary,
    "workspace_boundary",
    critical = true,
    default = RuleSeverity::Block,
    triggers = [PreTool, PreEdit]
);
rule!(
    SecretFileGuard,
    "secret_file_guard",
    critical = true,
    default = RuleSeverity::Warn,
    triggers = [PreTool, PreEdit]
);
rule!(
    TestFirstWhenConfigured,
    "test_first_when_configured",
    critical = false,
    default = RuleSeverity::Warn,
    triggers = [PreEdit]
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
    default = RuleSeverity::Block,
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

const PROHIBITED_COMMIT_TERMS: &[&str] = &["leaked", "source-map", "private endpoint"];

impl NoStaleUncommitted {
    fn check(&self, ctx: &RuleContext, severity: RuleSeverity) -> Verdict {
        if ctx.uncommitted_unrelated {
            at(
                severity,
                "unrelated uncommitted changes are present; commit or stash them first",
            )
        } else {
            Verdict::Allow
        }
    }
}

impl WorkspaceBoundary {
    fn check(&self, ctx: &RuleContext, severity: RuleSeverity) -> Verdict {
        if ctx.path_inside_workspace == Some(false) {
            at(
                severity,
                "the target path is outside the workspace boundary",
            )
        } else {
            Verdict::Allow
        }
    }
}

impl SecretFileGuard {
    fn check(&self, ctx: &RuleContext, severity: RuleSeverity) -> Verdict {
        if ctx.path_secret_like {
            at(severity, "the target looks like a secret-bearing file")
        } else {
            Verdict::Allow
        }
    }
}

impl TestFirstWhenConfigured {
    fn check(&self, ctx: &RuleContext, severity: RuleSeverity) -> Verdict {
        if ctx.test_first_required && ctx.editing_impl_before_tests {
            at(severity, "implementation is being edited before its test")
        } else {
            Verdict::Allow
        }
    }
}

impl SuiteGreen {
    fn check(&self, ctx: &RuleContext, severity: RuleSeverity) -> Verdict {
        if ctx.tests_passed == Some(false) {
            at(severity, "the configured test command did not pass")
        } else {
            Verdict::Allow
        }
    }
}

impl ProgressUpdated {
    fn check(&self, ctx: &RuleContext, severity: RuleSeverity) -> Verdict {
        if ctx.progress_reflects_completion == Some(false) {
            at(
                severity,
                "PROGRESS.md does not yet reflect the completed step",
            )
        } else {
            Verdict::Allow
        }
    }
}

impl CommitMessageClean {
    fn check(&self, ctx: &RuleContext, severity: RuleSeverity) -> Verdict {
        let Some(message) = &ctx.commit_message else {
            return Verdict::Allow;
        };
        let lower = message.to_ascii_lowercase();
        if contains_secret(message) || PROHIBITED_COMMIT_TERMS.iter().any(|t| lower.contains(t)) {
            at(
                severity,
                "the commit message contains a secret or a prohibited reference",
            )
        } else {
            Verdict::Allow
        }
    }
}

impl AttemptLimit {
    fn check(&self, ctx: &RuleContext, severity: RuleSeverity) -> Verdict {
        if ctx.max_attempts > 0 && ctx.attempts >= ctx.max_attempts {
            at(severity, "the per-step attempt limit was reached")
        } else {
            Verdict::Allow
        }
    }
}

impl QualityGate {
    fn check(&self, ctx: &RuleContext, severity: RuleSeverity) -> Verdict {
        gate_verdict(&ctx.gate_outcomes, severity)
    }
}

/// Reduce quality-gate outcomes to one verdict. A passing check contributes
/// `Allow`; a denied or un-runnable check blocks; a failing check maps by its
/// configured severity — explicit `block` (e.g. `audit`) blocks, `warn` warns,
/// `off` is ignored, and the default (no override) is `retry`, the actionable
/// path the loop feeds back to the model. The rule's own severity is a ceiling:
/// `warn` softens everything to a warning, `off` disables the gate.
fn gate_verdict(outcomes: &[CheckOutcome], rule_severity: RuleSeverity) -> Verdict {
    if rule_severity == RuleSeverity::Off {
        return Verdict::Allow;
    }
    let mut worst = Verdict::Allow;
    for outcome in outcomes {
        let verdict = outcome_verdict(outcome);
        if rank(&verdict) > rank(&worst) {
            worst = verdict;
        }
    }
    apply_ceiling(worst, rule_severity)
}

fn outcome_verdict(outcome: &CheckOutcome) -> Verdict {
    let name = &outcome.name;
    match outcome.status {
        CheckStatus::Passed => Verdict::Allow,
        CheckStatus::Denied => Verdict::Block(format!(
            "quality check `{name}` was denied by the permission engine"
        )),
        CheckStatus::Errored => Verdict::Block(format!("quality check `{name}` could not run")),
        CheckStatus::Failed => match outcome.severity {
            Some(RuleSeverity::Off) => Verdict::Allow,
            Some(RuleSeverity::Warn) => {
                Verdict::Warn(format!("quality check `{name}` reported findings"))
            }
            Some(RuleSeverity::Block) => {
                Verdict::Block(format!("quality check `{name}` reported blocking findings"))
            }
            None => Verdict::Retry(format!(
                "quality check `{name}` failed; fix the findings and retry"
            )),
        },
    }
}

/// Severity ordering for reducing many outcomes to the most severe verdict.
fn rank(verdict: &Verdict) -> u8 {
    match verdict {
        Verdict::Allow => 0,
        Verdict::Warn(_) => 1,
        Verdict::Retry(_) => 2,
        Verdict::Discard(_) => 3,
        Verdict::Block(_) => 4,
    }
}

/// Apply the rule-level severity as a ceiling on the reduced verdict.
fn apply_ceiling(verdict: Verdict, rule_severity: RuleSeverity) -> Verdict {
    match rule_severity {
        RuleSeverity::Block => verdict,
        RuleSeverity::Off => Verdict::Allow,
        RuleSeverity::Warn => match verdict {
            Verdict::Allow => Verdict::Allow,
            other => Verdict::Warn(
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
            Box::new(WorkspaceBoundary),
            Box::new(SecretFileGuard),
            Box::new(TestFirstWhenConfigured),
            Box::new(SuiteGreen),
            Box::new(ProgressUpdated),
            Box::new(CommitMessageClean),
            Box::new(AttemptLimit),
            Box::new(QualityGate),
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
    pub fn evaluate(&self, trigger: Trigger, ctx: &RuleContext) -> Vec<(&'static str, Verdict)> {
        let mut outcomes = Vec::new();
        for rule in &self.rules {
            if !rule.applies_to(trigger) {
                continue;
            }
            let severity = self.effective_severity(rule.as_ref());
            let verdict = rule.evaluate(ctx, severity);
            if verdict != Verdict::Allow {
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
        // workspace_boundary
        assert!(matches!(
            WorkspaceBoundary.evaluate(
                &RuleContext {
                    path_inside_workspace: Some(false),
                    ..Default::default()
                },
                RuleSeverity::Block
            ),
            Verdict::Block(_)
        ));
        // secret_file_guard
        assert!(matches!(
            SecretFileGuard.evaluate(
                &RuleContext {
                    path_secret_like: true,
                    ..Default::default()
                },
                RuleSeverity::Warn
            ),
            Verdict::Warn(_)
        ));
        // test_first_when_configured
        assert!(matches!(
            TestFirstWhenConfigured.evaluate(
                &RuleContext {
                    test_first_required: true,
                    editing_impl_before_tests: true,
                    ..Default::default()
                },
                RuleSeverity::Warn
            ),
            Verdict::Warn(_)
        ));
        // suite_green
        assert!(matches!(
            SuiteGreen.evaluate(
                &RuleContext {
                    tests_passed: Some(false),
                    ..Default::default()
                },
                RuleSeverity::Block
            ),
            Verdict::Block(_)
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
            Verdict::Block(_)
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
            Verdict::Block(_)
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
            Verdict::Block(_)
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
            .any(|(n, v)| *n == "no_stale_uncommitted" && matches!(v, Verdict::Warn(_))));
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
        severity: Option<RuleSeverity>,
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
            Verdict::Allow
        );
    }

    #[test]
    fn quality_gate_retries_a_failed_actionable_check() {
        let verdict = gate_verdict(
            &[gate_outcome("clippy", CheckStatus::Failed, None)],
            RuleSeverity::Block,
        );
        assert!(matches!(verdict, Verdict::Retry(_)));
    }

    #[test]
    fn quality_gate_blocks_failed_block_denied_and_errored() {
        for outcome in [
            gate_outcome("audit", CheckStatus::Failed, Some(RuleSeverity::Block)),
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
                gate_outcome("audit", CheckStatus::Failed, Some(RuleSeverity::Block)),
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
        assert!(matches!(verdict, Verdict::Warn(_)));
    }

    #[test]
    fn quality_gate_is_critical_and_cannot_be_disabled() {
        let engine = engine(&[("quality_gate", RuleSeverity::Off)]);
        assert_eq!(engine.effective_severity(&QualityGate), RuleSeverity::Block);
        let ctx = RuleContext {
            gate_outcomes: vec![gate_outcome(
                "audit",
                CheckStatus::Failed,
                Some(RuleSeverity::Block),
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
}
