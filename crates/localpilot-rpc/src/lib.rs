//! Headless drive for LocalPilot: newline-delimited JSON over stdio.
//!
//! Exposes the existing session runtime to IDEs, automation, and tests —
//! typed commands in, streamed session events out, permission asks answered
//! over the wire with the decision logic staying in the permission engine.
//! No server, no product SDK: the supported embedding surface remains the
//! in-process `SessionRuntime` (see docs/embedding.md).
#![forbid(unsafe_code)]

mod acp;
mod approver;
mod ask_view;
mod framing;
mod mcp;
mod protocol;
mod serve;

pub use acp::{serve_acp, ACP_PROTOCOL_VERSION};
pub use approver::{
    AskEvent, AskOutcome, AskRegistry, PendingAsk, Principal, ReplyRefusal, RpcApprover,
};
pub use ask_view::AskView;
pub use framing::{JsonRecordReader, LineFraming};
pub use mcp::{
    serve_mcp, DriverInterventionRecord, McpServeOptions, McpServeReport, DEFAULT_MCP_IDLE_TIMEOUT,
    MCP_PROTOCOL_VERSION,
};
pub use protocol::{
    AskDetail, AttachTarget, ClientCommand, ClientRecord, InputDisposition, PlanStepWire,
    RejectCode, ServerEvent, ServerRecord, CAPABILITY_ACKS, RPC_PROTOCOL_VERSION, SERVER_VERSION,
};
pub use serve::{map_event, serve, RpcError, ServeContext};
