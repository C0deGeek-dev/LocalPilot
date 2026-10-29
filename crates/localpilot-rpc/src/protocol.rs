//! The headless-drive wire protocol: newline-delimited JSON over stdio.
//!
//! One JSON object per LF-terminated line in each direction. Every record
//! carries an explicit protocol version; an optional `id` correlates a reply
//! with the command that caused it. The protocol exposes the *existing*
//! runtime — commands in, streamed session events out — and never widens what
//! the permission engine would allow.

use localpilot_core::SessionId;
use serde::{Deserialize, Serialize};

use crate::approver::{AskOutcome, Principal};

/// The current RPC protocol version. Version negotiation happens in `hello`:
/// a client built for a newer version than this build receives a typed error
/// rather than silently mismatched records.
pub const RPC_PROTOCOL_VERSION: u32 = 1;

/// This build's server/protocol crate version, reported in the
/// [`ServerEvent::Attached`] `server_version` field.
///
/// This is deliberately *not* a second negotiated protocol number. Wire
/// compatibility is gated by [`RPC_PROTOCOL_VERSION`]; additive fields instead
/// evolve under `#[serde(default)]` / `#[serde(skip_serializing_if)]`, so a
/// peer that predates a field simply omits it and both sides still parse. This
/// constant is a human-facing build string a bound connection can log or
/// display — it coexists with the version handshake rather than replacing it.
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The attach capability that opts a connection into command acknowledgements
/// (`accepted` / `queued` / `rejected`) and `permission_resolved` events. A
/// client that does not declare it receives none of these record types.
pub const CAPABILITY_ACKS: &str = "acks";

/// One client-to-agent record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientRecord {
    /// Protocol version the client speaks.
    pub v: u32,
    /// Optional correlation id, echoed on the direct reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub command: ClientCommand,
}

/// A typed command from the client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ClientCommand {
    /// Handshake: the agent replies with its protocol version and session info.
    Hello,
    /// Submit user input.
    Prompt {
        text: String,
        #[serde(default)]
        disposition: InputDisposition,
    },
    /// Cancel the running turn.
    Cancel,
    /// Answer a pending permission ask.
    PermissionReply { ask_id: String, allow: bool },
    /// Inspect session, harness-step, and permission state.
    Status,
    /// End the connection.
    Shutdown,
    /// Bind this connection to a session — open a new one, or resume one by id
    /// or name — as the first thing a connection does on the server transport
    /// (one connection names its session once, then is bound to it). The server
    /// confirms with [`ServerEvent::Attached`]. The stdio drive is
    /// one-session-per-process and never uses this handshake.
    Attach {
        target: AttachTarget,
        /// Optional protocol extensions this client understands (see
        /// [`CAPABILITY_ACKS`]). Omitted by older clients.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        capabilities: Vec<String>,
    },
}

/// Where a [`ClientCommand::Attach`] binds the connection: a brand-new session,
/// or an existing one resumed by id or by name. Internally tagged by `mode`,
/// mirroring the `type`-tagged style of the command and event enums.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AttachTarget {
    /// Open and bind a brand-new session.
    OpenNew,
    /// Resume and bind the session with this id.
    ResumeId { session_id: SessionId },
    /// Resume and bind the session carrying this name.
    ResumeName { name: String },
}

/// When queued input is admitted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputDisposition {
    /// Run now; rejected while a turn is already running (cancel first).
    #[default]
    Immediate,
    /// Inject into the running turn at the next safe provider-turn boundary;
    /// runs immediately when idle.
    Steer,
    /// Queue until the session is idle, then run as its own turn.
    FollowUp,
}

/// One agent-to-client record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerRecord {
    pub v: u32,
    /// Correlation id of the command this record directly replies to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub event: ServerEvent,
}

/// A typed event to the client. Streaming events mirror the runtime's session
/// events; the durable forms of the same vocabulary live in the session event
/// log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ServerEvent {
    Hello {
        protocol_version: u32,
        session_id: String,
        model: String,
    },
    /// Confirms a [`ClientCommand::Attach`]: the connection is now bound to
    /// `session_id`. `server_version` names the server build (see
    /// [`SERVER_VERSION`]); it is `#[serde(default)]` so a record written by an
    /// older server predating the field still deserializes (the field fills in
    /// empty), and skipped when empty so a default value never bloats the wire.
    Attached {
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        server_version: String,
    },
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    ToolStarted {
        id: String,
        name: String,
    },
    ToolFinished {
        id: String,
        name: String,
        is_error: bool,
        output: String,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    ContextUsage {
        used: usize,
        limit: usize,
    },
    /// Context was compacted automatically (the pre-request budget check or an
    /// overflow retry), not by a manual command: `dropped_exchanges` were trimmed
    /// and the projection now uses `context_used` of `limit` tokens.
    Compacted {
        dropped_exchanges: usize,
        context_used: usize,
        limit: usize,
    },
    Warning {
        message: String,
    },
    Plan {
        steps: Vec<PlanStepWire>,
    },
    QuotaPaused {
        reset: String,
    },
    Recovery {
        health: String,
    },
    /// The turn loop stopped; `reason` is the stop reason in snake case.
    Stopped {
        reason: String,
    },
    /// A permission decision needs the client: answer with
    /// `permission_reply`. An unanswered ask is denied, exactly like
    /// non-interactive mode.
    PermissionAsk {
        ask_id: String,
        tool: String,
        detail: String,
        risk: String,
        /// When the ask expires unanswered, in milliseconds since the Unix
        /// epoch. Zero (and omitted) from older servers.
        #[serde(default, skip_serializing_if = "is_zero")]
        expires_at_ms: u64,
    },
    /// An ask ended: answered by a client, expired, or cancelled with its turn.
    /// Sent to every attached client that declared [`CAPABILITY_ACKS`].
    PermissionResolved {
        ask_id: String,
        outcome: AskOutcome,
        /// Who answered; absent when the ask expired or was cancelled.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<Principal>,
    },
    /// Reply to `status`.
    Status {
        session_id: String,
        model: String,
        profile: String,
        busy: bool,
        pending_asks: Vec<String>,
        next_step: Option<String>,
        /// The pending asks with full detail, so a client that attached late
        /// can render them. Empty (and omitted) from older servers.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        asks: Vec<AskDetail>,
    },
    /// Queued input was accepted with the given disposition.
    Queued {
        disposition: InputDisposition,
    },
    /// A command was accepted and is being acted on now. Sent only to clients
    /// that declared [`CAPABILITY_ACKS`]. For a `cancel`,
    /// `dropped_follow_ups` counts the queued follow-ups it discarded.
    Accepted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dropped_follow_ups: Option<usize>,
    },
    /// The client fell behind and missed ask events. `asks` is the complete set
    /// of asks pending now: show exactly these, and drop any other ask still
    /// shown. Sent only to clients that declared [`CAPABILITY_ACKS`].
    AsksResync {
        asks: Vec<AskDetail>,
    },
    /// A command was refused; the connection stays open. Sent only to clients
    /// that declared [`CAPABILITY_ACKS`] (others receive [`ServerEvent::Error`]).
    Rejected {
        code: RejectCode,
        message: String,
    },
    /// A command could not be honored; the connection stays open.
    Error {
        message: String,
    },
    /// The agent is closing the connection.
    Closed,
    /// A tool has failed repeatedly; the agent is switching strategy.
    ToolStuck {
        name: String,
        count: u32,
    },
}

/// A pending permission ask with everything needed to render it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskDetail {
    pub ask_id: String,
    pub tool: String,
    pub detail: String,
    pub risk: String,
    pub expires_at_ms: u64,
}

/// Why a command was rejected. A closed set: clients may match on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectCode {
    /// A turn is already running; cancel it, steer, or send a follow-up.
    Busy,
    /// `cancel` with no turn running.
    NoTurnRunning,
    /// The follow-up queue is full.
    QueueFull,
    /// No such permission ask.
    UnknownAsk,
    /// The ask was already answered, expired, or cancelled.
    AlreadyResolved,
    /// The reply arrived after the ask's deadline.
    Expired,
    /// A second `attach` on a bound connection.
    AlreadyAttached,
    /// The session has been closed (reaped after inactivity); attach again to
    /// resume it.
    SessionClosed,
    /// The record could not be parsed.
    Malformed,
    /// This server does not understand the command.
    UnsupportedCommand,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// One plan step on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStepWire {
    pub title: String,
    pub status: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_records_roundtrip() {
        let records = vec![
            ClientRecord {
                v: RPC_PROTOCOL_VERSION,
                id: Some("1".to_string()),
                command: ClientCommand::Hello,
            },
            ClientRecord {
                v: RPC_PROTOCOL_VERSION,
                id: None,
                command: ClientCommand::Prompt {
                    text: "fix the bug".to_string(),
                    disposition: InputDisposition::Steer,
                },
            },
            ClientRecord {
                v: RPC_PROTOCOL_VERSION,
                id: None,
                command: ClientCommand::PermissionReply {
                    ask_id: "ask-1".to_string(),
                    allow: true,
                },
            },
        ];
        for record in records {
            let line = serde_json::to_string(&record).unwrap();
            let back: ClientRecord = serde_json::from_str(&line).unwrap();
            assert_eq!(record, back);
        }
    }

    #[test]
    fn disposition_defaults_to_immediate() {
        let record: ClientRecord =
            serde_json::from_str(r#"{"v":1,"command":{"type":"prompt","text":"hi"}}"#).unwrap();
        assert_eq!(
            record.command,
            ClientCommand::Prompt {
                text: "hi".to_string(),
                disposition: InputDisposition::Immediate,
            }
        );
    }

    #[test]
    fn server_records_roundtrip() {
        let record = ServerRecord {
            v: RPC_PROTOCOL_VERSION,
            id: None,
            event: ServerEvent::PermissionAsk {
                ask_id: "ask-1".to_string(),
                tool: "run_shell".to_string(),
                detail: "rm -rf build".to_string(),
                risk: "run a command".to_string(),
                expires_at_ms: 1_700_000_000_000,
            },
        };
        let line = serde_json::to_string(&record).unwrap();
        let back: ServerRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(record, back);
    }

    // --- attach handshake round-trips ---------------------------------------

    #[test]
    fn attach_commands_roundtrip_for_every_target() {
        let id = localpilot_core::SessionId::new();
        let records = vec![
            ClientRecord {
                v: RPC_PROTOCOL_VERSION,
                id: Some("a1".to_string()),
                command: ClientCommand::Attach {
                    target: AttachTarget::OpenNew,
                    capabilities: Vec::new(),
                },
            },
            ClientRecord {
                v: RPC_PROTOCOL_VERSION,
                id: None,
                command: ClientCommand::Attach {
                    target: AttachTarget::ResumeId { session_id: id },
                    capabilities: Vec::new(),
                },
            },
            ClientRecord {
                v: RPC_PROTOCOL_VERSION,
                id: None,
                command: ClientCommand::Attach {
                    target: AttachTarget::ResumeName {
                        name: "nightly".to_string(),
                    },
                    capabilities: vec![CAPABILITY_ACKS.to_string()],
                },
            },
        ];
        for record in records {
            let line = serde_json::to_string(&record).unwrap();
            let back: ClientRecord = serde_json::from_str(&line).unwrap();
            assert_eq!(record, back);
        }
    }

    #[test]
    fn attach_target_uses_the_mode_internal_tag() {
        let id = localpilot_core::SessionId::new();
        let json = serde_json::to_value(ClientCommand::Attach {
            target: AttachTarget::ResumeId { session_id: id },
            capabilities: Vec::new(),
        })
        .unwrap();
        assert_eq!(json["type"], "attach");
        assert_eq!(json["target"]["mode"], "resume_id");
        assert_eq!(json["target"]["session_id"], id.to_string());
    }

    #[test]
    fn attached_event_roundtrips_with_server_version() {
        let record = ServerRecord {
            v: RPC_PROTOCOL_VERSION,
            id: Some("a1".to_string()),
            event: ServerEvent::Attached {
                session_id: localpilot_core::SessionId::new(),
                server_version: SERVER_VERSION.to_string(),
            },
        };
        let line = serde_json::to_string(&record).unwrap();
        assert!(
            line.contains(SERVER_VERSION),
            "a populated server_version is written to the wire"
        );
        let back: ServerRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(record, back);
    }

    // --- additive evolution: older-shaped payloads still deserialize --------

    #[test]
    fn attached_payload_without_server_version_still_deserializes() {
        // An older server that predates `server_version` emits an `attached`
        // event without it. It must still parse into the current type, with the
        // field defaulted — forward/backward compatibility without a second
        // version handshake.
        let id = localpilot_core::SessionId::new();
        let older = format!(r#"{{"v":1,"event":{{"type":"attached","session_id":"{id}"}}}}"#);
        let record: ServerRecord = serde_json::from_str(&older).unwrap();
        assert_eq!(
            record.event,
            ServerEvent::Attached {
                session_id: id,
                server_version: String::new(),
            },
            "the missing field defaulted to empty instead of failing to parse"
        );
    }

    #[test]
    fn empty_server_version_is_omitted_from_the_wire() {
        // The skip-serializing half of the discipline: a default-valued field
        // does not bloat the wire, so it round-trips through the empty default.
        let event = ServerEvent::Attached {
            session_id: localpilot_core::SessionId::new(),
            server_version: String::new(),
        };
        let line = serde_json::to_string(&event).unwrap();
        assert!(
            !line.contains("server_version"),
            "an empty server_version must be skipped: {line}"
        );
        let back: ServerEvent = serde_json::from_str(&line).unwrap();
        assert_eq!(event, back);
    }

    // --- compatibility: pre-attach records are unaffected -------------------

    #[test]
    fn pre_attach_records_are_unaffected_by_the_new_variants() {
        // A record using none of the new variants still decodes from its
        // historical shape and re-encodes without any attach-related key: the
        // additions are purely additive to the existing stdio/ACP vocabulary.
        let client: ClientRecord =
            serde_json::from_str(r#"{"v":1,"command":{"type":"prompt","text":"run the tests"}}"#)
                .unwrap();
        let encoded = serde_json::to_string(&client).unwrap();
        assert!(
            !encoded.contains("attach"),
            "unexpected attach key: {encoded}"
        );
        assert_eq!(
            serde_json::from_str::<ClientRecord>(&encoded).unwrap(),
            client,
            "re-decoding a pre-attach client record is stable"
        );

        // A server `hello` has no defaulted/skipped fields, so it stays
        // byte-for-byte identical across a decode/encode round-trip.
        let hello =
            r#"{"v":1,"event":{"type":"hello","protocol_version":1,"session_id":"s","model":"m"}}"#;
        let decoded: ServerRecord = serde_json::from_str(hello).unwrap();
        let re_encoded = serde_json::to_string(&decoded).unwrap();
        assert_eq!(
            re_encoded, hello,
            "pre-attach server record is byte-identical"
        );
        assert!(!re_encoded.contains("attached"));
    }
}
