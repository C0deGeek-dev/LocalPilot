//! The durable, tree-shaped session event log.
//!
//! Every entry carries an `id`, a `parent_id`, and an explicit format version.
//! Normal execution forms a chain (each event's parent is the previous event);
//! a harness replan forks: the abandoned attempt closes with a
//! [`SessionEventKind::BranchClosed`] summary and the next attempt's
//! [`SessionEventKind::BranchForked`] points back at the last good ancestor,
//! so a replanned run is replayable and auditable from the log alone.
//!
//! Loading migrates older format versions on read (a version-0 line — written
//! before the explicit version field existed — is upgraded in memory); a line
//! with a *newer* version than this build understands is a typed error, never
//! a silent misparse.

use localpilot_core::{EventId, Message, StructuredSummary};
use serde::{Deserialize, Serialize};

/// The current session event-log format version.
pub const SESSION_EVENT_FORMAT_VERSION: u32 = 1;

/// One durable entry in a session's event log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEvent {
    /// Format version this entry was written with.
    pub v: u32,
    pub id: EventId,
    /// The event this one descends from; `None` only for the first entry.
    pub parent_id: Option<EventId>,
    /// Wall-clock seconds since the Unix epoch when the event was recorded.
    pub at_unix: u64,
    /// What happened. Nested (not flattened) so a kind's own fields can never
    /// collide with the envelope's `id`/`parent_id`.
    pub kind: SessionEventKind,
}

/// Why a session was opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenReason {
    New,
    Resumed,
    /// Opened as a fork of another session's history.
    Forked,
}

/// Where a transcript message came from. Derivable from the message itself so
/// the transcript can always be rebuilt from the event log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageOrigin {
    /// Typed by the user.
    UserInput,
    /// Produced by the model (text/reasoning/tool calls).
    Assistant,
    /// A tool's result.
    ToolResult,
    /// Setup or host-injected system content.
    System,
    /// Synthesized by the runtime (repair prompt, rejection feedback, budget
    /// notice); `why` records the reason.
    Synthetic { why: String },
    /// A user-initiated shell run surfaced into the transcript.
    Shell,
}

/// What happened. Growable; the format version covers shape changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SessionEventKind {
    SessionOpened {
        reason: OpenReason,
    },
    SessionClosed,
    /// A message entered the model-visible history. The transcript is exactly
    /// the ordered `Message` events, so it is derivable from the log.
    Message {
        message: Message,
        origin: MessageOrigin,
    },
    TurnStarted {
        model: String,
    },
    TurnEnded {
        stop: String,
    },
    UsageReported {
        input_tokens: u64,
        output_tokens: u64,
    },
    /// A permission decision on one effect. Recorded by the permission hook;
    /// until the hook fabric routes decisions through the runtime, entries of
    /// this kind may be absent.
    PermissionDecided {
        tool: String,
        decision: String,
        detail: String,
    },
    ToolStarted {
        id: String,
        name: String,
    },
    ToolFinished {
        id: String,
        name: String,
        is_error: bool,
    },
    RecoveryDiagnostic {
        kind: String,
        health: String,
    },
    QuotaPaused {
        reset: String,
    },
    QuotaResumed,
    /// Context compaction ran and trimmed history for the next request.
    Compacted {
        summary: StructuredSummary,
    },
    StepStarted {
        number: usize,
        description: String,
    },
    StepCompleted {
        number: usize,
        commit: Option<String>,
        attempts: u32,
    },
    /// An abandoned line of work (a discarded step attempt) closed with a
    /// structured digest of what was tried.
    BranchClosed {
        summary: StructuredSummary,
    },
    /// Work resumed from an earlier ancestor instead of the previous event.
    BranchForked {
        from: EventId,
    },
    Cancelled,
}

impl SessionEvent {
    /// Parse one event-log line, migrating older format versions on load.
    ///
    /// # Errors
    /// Returns [`super::StoreError`] if the line is not valid JSON, is a
    /// version this build does not know how to read, or fails to migrate.
    pub fn from_line(line: &str) -> Result<Self, super::StoreError> {
        let value: serde_json::Value = serde_json::from_str(line)?;
        let version = value
            .get("v")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        match u32::try_from(version) {
            Ok(v) if v <= SESSION_EVENT_FORMAT_VERSION => {
                let migrated = migrate(value, v)?;
                Ok(serde_json::from_value(migrated)?)
            }
            _ => Err(super::StoreError::UnsupportedFormat {
                found: version,
                supported: SESSION_EVENT_FORMAT_VERSION,
            }),
        }
    }
}

/// Migrate a raw event value from `from_version` up to the current version.
/// Each step is explicit so adding version N+1 means adding one match arm.
fn migrate(
    mut value: serde_json::Value,
    from_version: u32,
) -> Result<serde_json::Value, super::StoreError> {
    let mut version = from_version;
    while version < SESSION_EVENT_FORMAT_VERSION {
        value = match version {
            // v0 -> v1: identical shape; v0 lines simply predate the explicit
            // version field.
            0 => {
                if let serde_json::Value::Object(map) = &mut value {
                    map.insert("v".to_string(), serde_json::json!(1));
                }
                value
            }
            _ => {
                return Err(super::StoreError::UnsupportedFormat {
                    found: u64::from(version),
                    supported: SESSION_EVENT_FORMAT_VERSION,
                })
            }
        };
        version += 1;
    }
    Ok(value)
}

/// Rebuild the transcript — the exact model-visible message history — from an
/// event sequence: the ordered `Message` events.
#[must_use]
pub fn transcript_from_events(events: &[SessionEvent]) -> Vec<Message> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            SessionEventKind::Message { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

/// Derive a message's origin from the message itself, so emission sites cannot
/// disagree with the payload.
#[must_use]
pub fn origin_for(message: &Message) -> MessageOrigin {
    if let Some(why) = &message.metadata.synthetic {
        return MessageOrigin::Synthetic { why: why.clone() };
    }
    match message.role {
        localpilot_core::Role::User => MessageOrigin::UserInput,
        localpilot_core::Role::Assistant => MessageOrigin::Assistant,
        localpilot_core::Role::Tool => MessageOrigin::ToolResult,
        localpilot_core::Role::System => MessageOrigin::System,
        localpilot_core::Role::UserShell => MessageOrigin::Shell,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_core::Role;

    fn event(kind: SessionEventKind, parent: Option<EventId>) -> SessionEvent {
        SessionEvent {
            v: SESSION_EVENT_FORMAT_VERSION,
            id: EventId::new(),
            parent_id: parent,
            at_unix: 1,
            kind,
        }
    }

    #[test]
    fn every_event_kind_roundtrips_through_a_log_line() {
        let kinds = vec![
            SessionEventKind::SessionOpened {
                reason: OpenReason::New,
            },
            SessionEventKind::SessionClosed,
            SessionEventKind::Message {
                message: Message::text(Role::User, "hi"),
                origin: MessageOrigin::UserInput,
            },
            SessionEventKind::Message {
                message: Message::text(Role::User, "repair").into_synthetic("repair prompt"),
                origin: MessageOrigin::Synthetic {
                    why: "repair prompt".to_string(),
                },
            },
            SessionEventKind::TurnStarted {
                model: "m".to_string(),
            },
            SessionEventKind::TurnEnded {
                stop: "done".to_string(),
            },
            SessionEventKind::UsageReported {
                input_tokens: 3,
                output_tokens: 5,
            },
            SessionEventKind::PermissionDecided {
                tool: "run_shell".to_string(),
                decision: "ask".to_string(),
                detail: "rm -rf build".to_string(),
            },
            SessionEventKind::ToolStarted {
                id: "c1".to_string(),
                name: "read_file".to_string(),
            },
            SessionEventKind::ToolFinished {
                id: "c1".to_string(),
                name: "read_file".to_string(),
                is_error: false,
            },
            SessionEventKind::RecoveryDiagnostic {
                kind: "slash_flood".to_string(),
                health: "suspect".to_string(),
            },
            SessionEventKind::QuotaPaused {
                reset: "retry in ~30s".to_string(),
            },
            SessionEventKind::QuotaResumed,
            SessionEventKind::Compacted {
                summary: StructuredSummary::new("trimmed:", vec!["x".to_string()]),
            },
            SessionEventKind::StepStarted {
                number: 1,
                description: "write tests".to_string(),
            },
            SessionEventKind::StepCompleted {
                number: 1,
                commit: Some("abc123".to_string()),
                attempts: 1,
            },
            SessionEventKind::BranchClosed {
                summary: StructuredSummary::new("closed:", vec!["attempt 1 failed".to_string()]),
            },
            SessionEventKind::BranchForked {
                from: EventId::new(),
            },
            SessionEventKind::Cancelled,
        ];
        let mut parent = None;
        for kind in kinds {
            let original = event(kind, parent);
            parent = Some(original.id);
            let line = serde_json::to_string(&original).unwrap();
            let back = SessionEvent::from_line(&line).unwrap();
            assert_eq!(original, back);
        }
    }

    #[test]
    fn a_version_zero_line_migrates_on_load() {
        let original = event(
            SessionEventKind::TurnStarted {
                model: "m".to_string(),
            },
            None,
        );
        let mut value = serde_json::to_value(&original).unwrap();
        value.as_object_mut().unwrap().remove("v");
        let line = serde_json::to_string(&value).unwrap();

        let loaded = SessionEvent::from_line(&line).unwrap();
        assert_eq!(loaded.v, SESSION_EVENT_FORMAT_VERSION);
        assert_eq!(loaded.kind, original.kind);
    }

    #[test]
    fn a_newer_version_is_a_typed_error_not_a_misparse() {
        let mut value = serde_json::to_value(event(SessionEventKind::SessionClosed, None)).unwrap();
        value["v"] = serde_json::json!(SESSION_EVENT_FORMAT_VERSION + 1);
        let line = serde_json::to_string(&value).unwrap();
        assert!(matches!(
            SessionEvent::from_line(&line),
            Err(super::super::StoreError::UnsupportedFormat { .. })
        ));
    }

    #[test]
    fn transcript_is_the_ordered_message_events() {
        let user = Message::text(Role::User, "hi");
        let assistant = Message::text(Role::Assistant, "hello");
        let events = vec![
            event(
                SessionEventKind::SessionOpened {
                    reason: OpenReason::New,
                },
                None,
            ),
            event(
                SessionEventKind::Message {
                    message: user.clone(),
                    origin: MessageOrigin::UserInput,
                },
                None,
            ),
            event(
                SessionEventKind::TurnStarted {
                    model: "m".to_string(),
                },
                None,
            ),
            event(
                SessionEventKind::Message {
                    message: assistant.clone(),
                    origin: MessageOrigin::Assistant,
                },
                None,
            ),
        ];
        assert_eq!(transcript_from_events(&events), vec![user, assistant]);
    }

    #[test]
    fn origin_is_derived_from_the_message() {
        assert_eq!(
            origin_for(&Message::text(Role::User, "x")),
            MessageOrigin::UserInput
        );
        assert_eq!(
            origin_for(&Message::text(Role::Tool, "x")),
            MessageOrigin::ToolResult
        );
        assert_eq!(
            origin_for(&Message::text(Role::User, "x").into_synthetic("repair prompt")),
            MessageOrigin::Synthetic {
                why: "repair prompt".to_string()
            }
        );
    }
}
