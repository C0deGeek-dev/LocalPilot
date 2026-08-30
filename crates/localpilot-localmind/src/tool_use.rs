//! Adapter from a LocalPilot verifier verdict to a LocalMind tool-use lesson
//! candidate.
//!
//! The verdict is a **fact taken from the event log** — the `ToolVerified`
//! records the host writes — not re-parsed transcript prose (ADR-0018's
//! structured-signal import). The host fills [`ToolUseSignal`] from the session
//! evidence and calls [`tool_use_candidate`]; promotion is gated on the verified
//! trajectory by `localmind_core::promote_tool_use`, and the evidence is redacted
//! before it can be persisted.

use localmind_core::{
    promote_tool_use, CandidateLesson, Confidence, EvidenceKind, EvidenceRef, FailureRecovery,
    InvalidationRule, LessonId, LessonScope, ToolUseLesson, ToolUseTrajectory,
};

/// A structured tool-use signal a host extracts from a closed session. Every
/// field is a fact keyed by the event log, not a re-parse of prose.
#[derive(Clone, Debug)]
pub struct ToolUseSignal {
    pub session: String,
    pub tool: String,
    pub tool_version: u32,
    /// The verifier verdict for the trajectory's tool calls was `Verified`.
    pub verified: bool,
    /// The trajectory ended degraded or in a tool loop.
    pub degraded_or_looping: bool,
    pub context_cues: Vec<String>,
    pub preconditions: Vec<String>,
    pub action_sequence: Vec<String>,
    pub expected_observations: Vec<String>,
    pub verification: String,
    /// Observed (failure, recovery) pairs.
    pub failure_recovery: Vec<(String, String)>,
    pub confidence: f32,
    /// A short evidence reference; redacted before persistence.
    pub evidence: String,
}

/// Build a tool-use lesson candidate from a host verdict signal, or `None` when
/// the trajectory was not verified (it stays episodic). The verdict reaches the
/// candidate as a fact; the evidence is redacted before it can be persisted.
#[must_use]
pub fn tool_use_candidate(signal: &ToolUseSignal) -> Option<CandidateLesson> {
    let confidence = Confidence::new(signal.confidence.clamp(0.0, 1.0)).ok()?;
    let lesson = ToolUseLesson {
        context_cues: signal.context_cues.clone(),
        tool: signal.tool.clone(),
        tool_version: signal.tool_version,
        preconditions: signal.preconditions.clone(),
        action_sequence: signal.action_sequence.clone(),
        expected_observations: signal.expected_observations.clone(),
        verification: signal.verification.clone(),
        failure_recovery: signal
            .failure_recovery
            .iter()
            .map(|(failure, recovery)| FailureRecovery {
                failure: failure.clone(),
                recovery: recovery.clone(),
            })
            .collect(),
        confidence,
        provenance: signal.session.clone(),
        last_verified: None,
        invalidation: InvalidationRule::OnToolVersionBump,
        scope: LessonScope::Project,
    };
    let trajectory = ToolUseTrajectory {
        id: LessonId::new(format!("tooluse:{}:{}", signal.session, signal.tool)),
        summary: format!("verified tool-use pattern for `{}`", signal.tool),
        verified: signal.verified,
        degraded_or_looping: signal.degraded_or_looping,
        lesson,
        // Redact before the candidate can reach the review queue / store.
        evidence: EvidenceRef::new(EvidenceKind::Transcript, &signal.evidence).redacted(),
    };
    promote_tool_use(&trajectory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use localmind_core::LessonCategory;

    fn signal(verified: bool) -> ToolUseSignal {
        ToolUseSignal {
            session: "session-1".to_string(),
            tool: "write_file".to_string(),
            tool_version: 1,
            verified,
            degraded_or_looping: false,
            context_cues: vec!["overwrite an existing config".to_string()],
            preconditions: vec!["read the file first".to_string()],
            action_sequence: vec!["read_file".to_string(), "write_file".to_string()],
            expected_observations: vec!["the file exists with the new content".to_string()],
            verification: "read back confirms".to_string(),
            failure_recovery: vec![("write rejected".to_string(), "read then retry".to_string())],
            confidence: 0.8,
            evidence: "raw transcript line".to_string(),
        }
    }

    #[test]
    fn a_verified_signal_reaches_the_candidate_with_redacted_evidence() {
        let candidate = tool_use_candidate(&signal(true)).expect("verified -> candidate");
        assert_eq!(candidate.category, LessonCategory::ToolUse);
        let lesson = candidate
            .tool_use
            .as_ref()
            .expect("carries the tool-use lesson");
        assert_eq!(lesson.tool, "write_file");
        assert_eq!(lesson.action_sequence, vec!["read_file", "write_file"]);
        // Redaction-before-persistence: the evidence is marked redacted.
        assert!(candidate.evidence().iter().all(|e| e.redacted));
    }

    #[test]
    fn an_unverified_signal_yields_no_candidate() {
        assert!(tool_use_candidate(&signal(false)).is_none());
    }
}
