#![allow(clippy::unwrap_used, clippy::expect_used)]

use localpilot_recovery::{BadOutputKind, ModelHealth, RecoveryAction, RecoveryDiagnostic};

#[test]
fn retired_and_future_diagnostic_kinds_remain_readable() {
    for kind in ["tool_call_loop", "repeated_transient_error", "future_kind"] {
        let historic = serde_json::json!({
            "kind": kind,
            "attempt": 2,
            "health": "degraded",
            "actions": ["save_diagnostic", "mark_degraded", "stop_harness_progress"]
        });
        let diagnostic: RecoveryDiagnostic = serde_json::from_value(historic).unwrap();
        assert_eq!(diagnostic.kind, BadOutputKind::Unknown);
        assert_eq!(serde_json::to_value(diagnostic.kind).unwrap(), "unknown");
        assert_eq!(diagnostic.attempt, 2);
        assert_eq!(diagnostic.health, ModelHealth::Degraded);
        assert_eq!(
            diagnostic.actions,
            vec![
                RecoveryAction::SaveDiagnostic,
                RecoveryAction::MarkDegraded,
                RecoveryAction::StopHarnessProgress,
            ]
        );
        let normalized = serde_json::to_value(&diagnostic).unwrap();
        assert_eq!(
            serde_json::from_value::<RecoveryDiagnostic>(normalized).unwrap(),
            diagnostic
        );
    }
}

#[test]
fn live_diagnostic_kinds_keep_their_wire_format() {
    for (kind, spelling) in [
        (BadOutputKind::EmptyTurn, "empty_turn"),
        (BadOutputKind::RepeatedTokenLoop, "repeated_token_loop"),
        (BadOutputKind::SlashFlood, "slash_flood"),
        (BadOutputKind::MalformedToolCall, "malformed_tool_call"),
        (
            BadOutputKind::MalformedStructuredOutput,
            "malformed_structured_output",
        ),
    ] {
        let diagnostic = RecoveryDiagnostic {
            kind,
            attempt: 1,
            health: ModelHealth::Recovering,
            actions: vec![RecoveryAction::SaveDiagnostic],
        };
        let json = serde_json::to_value(&diagnostic).unwrap();
        assert_eq!(json["kind"], spelling);
        assert_eq!(
            serde_json::from_value::<RecoveryDiagnostic>(json).unwrap(),
            diagnostic
        );
    }
}

#[test]
fn malformed_diagnostics_are_not_hidden_by_the_unknown_kind_fallback() {
    for kind in [serde_json::Value::Null, serde_json::json!(42)] {
        let malformed = serde_json::json!({
            "kind": kind,
            "attempt": 1,
            "health": "recovering",
            "actions": ["save_diagnostic"]
        });
        assert!(serde_json::from_value::<RecoveryDiagnostic>(malformed).is_err());
    }
    let missing_kind = serde_json::json!({
        "attempt": 1,
        "health": "recovering",
        "actions": ["save_diagnostic"]
    });
    assert!(serde_json::from_value::<RecoveryDiagnostic>(missing_kind).is_err());
}
