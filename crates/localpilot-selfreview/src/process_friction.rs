//! Auto-captured session friction: the deterministic half of the harness-friction
//! observe-channel (the follow-up to the model-self-reported [`crate::friction`]).
//!
//! The capability scorecard's `process` block already derives, deterministically
//! from the session event log (no model), how a run actually behaved: how many
//! tool calls it made, how many repeated an earlier identical call, whether it
//! observed before editing, whether it tested before claiming done, whether it
//! recovered from a failure, and why it stopped. [`process_friction_findings`]
//! projects those measured signals into the same [`Finding`] shape the repo scan
//! and the audit-prompt friction use, so all three rank together. Read-only: it
//! maps numbers into advisory findings and writes nothing.
//!
//! The input [`ProcessFriction`] deserializes directly from a scorecard's
//! `process` object (matching field names), so the host can feed it a captured
//! scorecard without this crate depending on the harness.

use serde::Deserialize;

use crate::finding::{Finding, FindingKind, Severity};

/// The measured per-run process signals this projection reads, deserialized from a
/// capability scorecard's `process` block. Every field defaults so a partial or
/// empty block degrades to "no run, no findings" rather than an error.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProcessFriction {
    /// Total tool calls the run made. Zero means there was no run to grade, so no
    /// friction is emitted (the booleans below are only meaningful for a real run).
    #[serde(default)]
    pub tool_calls: u32,
    /// Calls that repeated an earlier identical `(tool, arguments)` call.
    #[serde(default)]
    pub redundant_calls: u32,
    /// An observation preceded the first mutating call (vacuously true with no
    /// edit). `false` means an edit was made before anything was observed.
    #[serde(default)]
    pub reproduce_before_fix: bool,
    /// A test-like call appeared before the run claimed done.
    #[serde(default)]
    pub test_before_done: bool,
    /// A failed call was followed by a later grounded success (the run recovered).
    #[serde(default)]
    pub recovered_after_failure: bool,
    /// The recorded stop label (e.g. `Done`, `BudgetExceeded`, `NoProgress`).
    #[serde(default)]
    pub exit_reason: String,
}

/// Project measured process signals into advisory [`FindingKind::Friction`]
/// findings. Deterministic and conservative: each rule fires only on a real run
/// (`tool_calls > 0`) and on a clean run (no redundancy, observed-then-edited,
/// tested, stopped on `Done`) it emits nothing. The findings carry the `agent`
/// owner, matching the audit-prompt friction source, and rank by the same
/// severity × confidence as every other finding.
#[must_use]
pub fn process_friction_findings(signals: &ProcessFriction) -> Vec<Finding> {
    let mut findings = Vec::new();
    if signals.tool_calls == 0 {
        return findings; // no run to grade
    }

    if signals.redundant_calls > 0 {
        // Repeated identical calls are wasted work; heavy thrash is more serious.
        let severity = if signals.redundant_calls >= 3 {
            Severity::High
        } else {
            Severity::Medium
        };
        findings.push(
            Finding::new(
                FindingKind::Friction,
                severity,
                0.95,
                format!(
                    "{} of {} tool call(s) repeated an earlier identical call (redundant work)",
                    signals.redundant_calls, signals.tool_calls
                ),
            )
            .owned_by("agent"),
        );
    }

    if is_friction_exit(&signals.exit_reason) {
        findings.push(
            Finding::new(
                FindingKind::Friction,
                Severity::High,
                0.95,
                format!(
                    "the run stopped on '{}', not completion",
                    signals.exit_reason
                ),
            )
            .owned_by("agent"),
        );
    }

    if !signals.reproduce_before_fix {
        findings.push(
            Finding::new(
                FindingKind::Friction,
                Severity::Medium,
                0.7,
                "an edit was made before any observation (no reproduce-before-fix)".to_string(),
            )
            .owned_by("agent"),
        );
    }

    if !signals.test_before_done {
        findings.push(
            Finding::new(
                FindingKind::Friction,
                Severity::Low,
                0.6,
                "the run claimed done with no test run during the task".to_string(),
            )
            .owned_by("agent"),
        );
    }

    if signals.recovered_after_failure {
        // Recovery is good, but a mid-task failure is friction worth surfacing.
        findings.push(
            Finding::new(
                FindingKind::Friction,
                Severity::Info,
                0.9,
                "a tool call failed mid-task; the run recovered, but the failure is friction"
                    .to_string(),
            )
            .owned_by("agent"),
        );
    }

    findings
}

/// Whether a stop label indicates the run hit a wall rather than completing.
fn is_friction_exit(exit_reason: &str) -> bool {
    matches!(
        exit_reason.trim().to_ascii_lowercase().as_str(),
        "budgetexceeded" | "budget_exceeded" | "noprogress" | "no_progress"
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// A clean run (worked, no redundancy, observed then edited, tested, finished)
    /// produces no friction.
    #[test]
    fn a_clean_run_has_no_friction() {
        let clean = ProcessFriction {
            tool_calls: 5,
            redundant_calls: 0,
            reproduce_before_fix: true,
            test_before_done: true,
            recovered_after_failure: false,
            exit_reason: "Done".to_string(),
        };
        assert!(process_friction_findings(&clean).is_empty());
    }

    /// An empty/default block (no run) emits nothing rather than firing the
    /// false-defaulting boolean rules.
    #[test]
    fn no_run_emits_nothing() {
        assert!(process_friction_findings(&ProcessFriction::default()).is_empty());
    }

    #[test]
    fn redundancy_is_flagged_and_scales_with_thrash() {
        let light = ProcessFriction {
            tool_calls: 6,
            redundant_calls: 1,
            reproduce_before_fix: true,
            test_before_done: true,
            exit_reason: "Done".to_string(),
            ..ProcessFriction::default()
        };
        let f = process_friction_findings(&light);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].kind, FindingKind::Friction);
        assert_eq!(f[0].severity, Severity::Medium);

        let heavy = ProcessFriction {
            redundant_calls: 4,
            ..light
        };
        assert_eq!(
            process_friction_findings(&heavy)[0].severity,
            Severity::High,
            "heavy thrash is more serious"
        );
    }

    #[test]
    fn a_friction_exit_is_high_severity() {
        for reason in ["BudgetExceeded", "no_progress"] {
            let signals = ProcessFriction {
                tool_calls: 3,
                reproduce_before_fix: true,
                test_before_done: true,
                exit_reason: reason.to_string(),
                ..ProcessFriction::default()
            };
            let f = process_friction_findings(&signals);
            assert_eq!(f.len(), 1, "{reason}");
            assert_eq!(f[0].severity, Severity::High, "{reason}");
            assert!(f[0].evidence.contains(reason));
        }
    }

    #[test]
    fn discipline_gaps_and_recovery_are_flagged() {
        let signals = ProcessFriction {
            tool_calls: 4,
            redundant_calls: 0,
            reproduce_before_fix: false, // edited before observing
            test_before_done: false,     // claimed done untested
            recovered_after_failure: true,
            exit_reason: "Done".to_string(),
        };
        let kinds: Vec<Severity> = process_friction_findings(&signals)
            .iter()
            .map(|f| f.severity)
            .collect();
        // one Medium (no-repro), one Low (no-test), one Info (recovered).
        assert!(kinds.contains(&Severity::Medium));
        assert!(kinds.contains(&Severity::Low));
        assert!(kinds.contains(&Severity::Info));
    }

    /// The input deserializes straight from a scorecard `process` object.
    #[test]
    fn deserializes_from_a_scorecard_process_block() {
        let json = r#"{"tool_calls":7,"redundant_calls":2,"reproduce_before_fix":true,
            "test_before_done":true,"retrieval_used":true,"retrieval_count":1,
            "exit_reason":"Done","recovered_after_failure":false,"discipline":null}"#;
        let signals: ProcessFriction = serde_json::from_str(json).unwrap();
        assert_eq!(signals.tool_calls, 7);
        assert_eq!(signals.redundant_calls, 2);
        // Unknown fields (retrieval_*, discipline) are ignored.
        assert_eq!(process_friction_findings(&signals).len(), 1);
    }
}
