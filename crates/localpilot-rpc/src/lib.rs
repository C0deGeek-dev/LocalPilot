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
mod framing;
mod mcp;
mod protocol;
mod serve;

pub use acp::{serve_acp, ACP_PROTOCOL_VERSION};
pub use approver::{AskRegistry, PendingAsk, RpcApprover};
pub use framing::LineFraming;
pub use mcp::{
    serve_mcp, DriverInterventionRecord, McpServeOptions, McpServeReport, MCP_PROTOCOL_VERSION,
};
pub use protocol::{
    ClientCommand, ClientRecord, InputDisposition, PlanStepWire, ServerEvent, ServerRecord,
    RPC_PROTOCOL_VERSION,
};
pub use serve::{serve, RpcError, ServeContext};
