//! Provider quota window tracking and wait/resume scheduling for LocalPilot.
//!
//! Owns quota-window estimation from provider metadata, persistence of paused
//! harness runs, and the wait/resume policy and safety gates. It coordinates
//! with the harness, which keeps the committed state and plan authoritative
//! across a pause. Waiting honours provider-stated windows; it is never framed
//! as bypassing a limit.
#![forbid(unsafe_code)]

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use localpilot_config::{QuotaAutoResume, QuotaConfig};
use localpilot_llm::QuotaInfo;

/// An estimated wait window before a paused run may be re-probed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaWindow {
    /// How long to wait before re-probing.
    pub wait: Duration,
    /// Absolute reset time as a Unix timestamp, if the provider stated one.
    pub reset_at: Option<u64>,
    /// Whether the provider indicates the request is safe to retry after waiting.
    pub retryable: bool,
    /// A human-readable reason for the pause.
    pub reason: String,
}

const MIN_BACKOFF: Duration = Duration::from_secs(15);
const MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);

/// Estimate a wait window from provider quota metadata. A stated `retry_after`
/// wins; otherwise a stated `reset_at`; otherwise bounded exponential backoff
/// with jitter so the run re-probes rather than hammering the provider.
#[must_use]
pub fn estimate_window(info: &QuotaInfo, attempt: u32) -> QuotaWindow {
    let now = now_unix();
    let wait = if let Some(retry_after) = info.retry_after {
        retry_after
    } else if let Some(reset_at) = info.reset_at {
        Duration::from_secs(reset_at.saturating_sub(now))
    } else {
        backoff_with_jitter(attempt)
    };

    QuotaWindow {
        wait,
        reset_at: info.reset_at,
        retryable: info.retryable,
        reason: info
            .raw_provider_code
            .clone()
            .or_else(|| info.limit_kind.clone())
            .unwrap_or_else(|| "provider quota or rate limit".to_string()),
    }
}

fn backoff_with_jitter(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(16);
    let base = MIN_BACKOFF.saturating_mul(1u32 << shift).min(MAX_BACKOFF);
    let half = base / 2;
    half + half.mul_f64(jitter_fraction())
}

fn jitter_fraction() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    f64::from(nanos) / f64::from(u32::from(u16::MAX) + 1) % 1.0
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A persisted record of a paused harness run, written as an inspectable file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PausedRun {
    pub paused_at_unix: u64,
    pub reason: String,
    pub resume_eligible_unix: Option<u64>,
    pub step_number: usize,
    pub provider_id: String,
}

impl PausedRun {
    /// Record a pause now, scheduling the eligible resume time from a window.
    #[must_use]
    pub fn new(step_number: usize, provider_id: impl Into<String>, window: &QuotaWindow) -> Self {
        let now = now_unix();
        Self {
            paused_at_unix: now,
            reason: window.reason.clone(),
            resume_eligible_unix: Some(now + window.wait.as_secs()),
            step_number,
            provider_id: provider_id.into(),
        }
    }
}

/// How a paused run may resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeMode {
    /// Never auto-resume.
    Off,
    /// Prompt before waiting/resuming.
    Ask,
    /// Wait and resume automatically for this run.
    Run,
    /// Always resume eligible paused runs unattended. Requires explicit config.
    Global,
}

impl From<QuotaAutoResume> for ResumeMode {
    fn from(value: QuotaAutoResume) -> Self {
        match value {
            QuotaAutoResume::Off => ResumeMode::Off,
            QuotaAutoResume::Ask => ResumeMode::Ask,
            QuotaAutoResume::Run => ResumeMode::Run,
            QuotaAutoResume::Global => ResumeMode::Global,
        }
    }
}

/// The resume policy resolved from configuration.
#[derive(Debug, Clone)]
pub struct ResumePolicy {
    pub mode: ResumeMode,
    pub max_wait: Duration,
    pub requires_clean_workspace: bool,
    pub requires_no_pending_approval: bool,
    pub only_at_step_boundary: bool,
}

impl From<&QuotaConfig> for ResumePolicy {
    fn from(config: &QuotaConfig) -> Self {
        Self {
            mode: config.auto_resume.into(),
            max_wait: Duration::from_secs(u64::from(config.max_wait_minutes) * 60),
            requires_clean_workspace: config.resume_requires_clean_workspace,
            requires_no_pending_approval: config.resume_requires_no_pending_approval,
            only_at_step_boundary: config.resume_only_at_step_boundary,
        }
    }
}

/// The live conditions evaluated by the safety gates.
#[derive(Debug, Clone, Copy)]
pub struct ResumeContext {
    pub window_elapsed: bool,
    pub at_step_boundary: bool,
    pub workspace_clean: bool,
    pub pending_destructive_approval: bool,
    pub user_cancelled: bool,
    pub provider_identity_changed: bool,
    pub waited: Duration,
}

/// The resume decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeDecision {
    /// Resume now.
    Resume,
    /// Keep waiting; the window has not elapsed.
    Wait,
    /// Prompt the user (under `ask`).
    AskUser,
    /// A safety gate blocks resume, with the reason.
    BlockedBy(String),
}

/// Decide whether a paused run may resume, applying every safety gate before the
/// mode is consulted. Resume is conservative: any gate that fails blocks it.
#[must_use]
pub fn decide_resume(policy: &ResumePolicy, ctx: &ResumeContext) -> ResumeDecision {
    if ctx.user_cancelled {
        return ResumeDecision::BlockedBy("the run was cancelled by the user".to_string());
    }
    if ctx.provider_identity_changed {
        return ResumeDecision::BlockedBy(
            "the provider configuration changed during the wait".to_string(),
        );
    }
    if ctx.waited > policy.max_wait {
        return ResumeDecision::BlockedBy("the maximum wait time was exceeded".to_string());
    }
    if !ctx.window_elapsed {
        return ResumeDecision::Wait;
    }
    if policy.only_at_step_boundary && !ctx.at_step_boundary {
        return ResumeDecision::BlockedBy("not at a harness step boundary".to_string());
    }
    if policy.requires_no_pending_approval && ctx.pending_destructive_approval {
        return ResumeDecision::BlockedBy("a destructive action is pending approval".to_string());
    }
    if policy.requires_clean_workspace && !ctx.workspace_clean {
        return ResumeDecision::BlockedBy(
            "the workspace has unrelated uncommitted changes".to_string(),
        );
    }
    match policy.mode {
        ResumeMode::Off => ResumeDecision::BlockedBy("auto-resume is off".to_string()),
        ResumeMode::Ask => ResumeDecision::AskUser,
        ResumeMode::Run | ResumeMode::Global => ResumeDecision::Resume,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> QuotaInfo {
        QuotaInfo {
            retry_after: Some(Duration::from_secs(30)),
            reset_at: Some(now_unix() + 30),
            limit_kind: Some("requests".to_string()),
            retryable: true,
            raw_provider_code: Some("rate_limit_exceeded".to_string()),
        }
    }

    fn ready_ctx() -> ResumeContext {
        ResumeContext {
            window_elapsed: true,
            at_step_boundary: true,
            workspace_clean: true,
            pending_destructive_approval: false,
            user_cancelled: false,
            provider_identity_changed: false,
            waited: Duration::from_secs(30),
        }
    }

    fn policy(mode: ResumeMode) -> ResumePolicy {
        ResumePolicy {
            mode,
            max_wait: Duration::from_secs(3600),
            requires_clean_workspace: true,
            requires_no_pending_approval: true,
            only_at_step_boundary: true,
        }
    }

    #[test]
    fn estimate_window_prefers_retry_after() {
        let window = estimate_window(&info(), 1);
        assert_eq!(window.wait, Duration::from_secs(30));
        assert!(window.retryable);
        assert_eq!(window.reason, "rate_limit_exceeded");
    }

    #[test]
    fn estimate_window_falls_back_to_bounded_backoff() {
        let bare = QuotaInfo {
            retryable: true,
            ..QuotaInfo::default()
        };
        let window = estimate_window(&bare, 3);
        assert!(window.wait >= MIN_BACKOFF / 2);
        assert!(window.wait <= MAX_BACKOFF);
    }

    #[test]
    fn paused_run_round_trips_as_json() {
        let run = PausedRun::new(2, "openai", &estimate_window(&info(), 1));
        let json = serde_json::to_string(&run).unwrap();
        let back: PausedRun = serde_json::from_str(&json).unwrap();
        assert_eq!(run, back);
        assert_eq!(back.step_number, 2);
    }

    #[test]
    fn global_is_off_unless_explicitly_configured() {
        // The config default maps to Off, never Global.
        let default_mode: ResumeMode = QuotaConfig::default().auto_resume.into();
        assert_eq!(default_mode, ResumeMode::Off);
    }

    #[test]
    fn run_and_global_resume_when_safe() {
        assert_eq!(
            decide_resume(&policy(ResumeMode::Run), &ready_ctx()),
            ResumeDecision::Resume
        );
        assert_eq!(
            decide_resume(&policy(ResumeMode::Global), &ready_ctx()),
            ResumeDecision::Resume
        );
        assert_eq!(
            decide_resume(&policy(ResumeMode::Ask), &ready_ctx()),
            ResumeDecision::AskUser
        );
        assert!(matches!(
            decide_resume(&policy(ResumeMode::Off), &ready_ctx()),
            ResumeDecision::BlockedBy(_)
        ));
    }

    #[test]
    fn each_safety_gate_blocks_resume() {
        let cases = [
            ResumeContext {
                window_elapsed: false,
                ..ready_ctx()
            },
            ResumeContext {
                at_step_boundary: false,
                ..ready_ctx()
            },
            ResumeContext {
                workspace_clean: false,
                ..ready_ctx()
            },
            ResumeContext {
                pending_destructive_approval: true,
                ..ready_ctx()
            },
            ResumeContext {
                user_cancelled: true,
                ..ready_ctx()
            },
            ResumeContext {
                provider_identity_changed: true,
                ..ready_ctx()
            },
        ];
        for ctx in cases {
            let decision = decide_resume(&policy(ResumeMode::Global), &ctx);
            assert_ne!(
                decision,
                ResumeDecision::Resume,
                "ctx must not resume: {ctx:?}"
            );
        }
    }

    #[test]
    fn continuous_run_pauses_then_resumes_across_a_window() {
        let policy = policy(ResumeMode::Global);
        // Mid-window: wait.
        let waiting = ResumeContext {
            window_elapsed: false,
            ..ready_ctx()
        };
        assert_eq!(decide_resume(&policy, &waiting), ResumeDecision::Wait);
        // After the window, at a safe boundary: resume.
        assert_eq!(decide_resume(&policy, &ready_ctx()), ResumeDecision::Resume);
        // But a pending destructive approval still blocks it.
        let pending = ResumeContext {
            pending_destructive_approval: true,
            ..ready_ctx()
        };
        assert!(matches!(
            decide_resume(&policy, &pending),
            ResumeDecision::BlockedBy(_)
        ));
    }
}
