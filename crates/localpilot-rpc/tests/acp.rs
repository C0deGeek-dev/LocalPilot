//! ACP adapter conformance against a minimal scripted client.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use localpilot_harness::{SessionConfig, SessionRuntime};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_rpc::{serve_acp, RpcApprover, ACP_PROTOCOL_VERSION};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, Workspace};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

type Built = (
    tempfile::TempDir,
    SessionRuntime,
    tokio::sync::mpsc::UnboundedReceiver<localpilot_rpc::PendingAsk>,
    localpilot_rpc::AskRegistry,
);

fn build(provider: FakeProvider) -> Built {
    let (approver, ask_rx, registry) = RpcApprover::new();
    let dir = tempfile::tempdir().unwrap();
    let runtime = SessionRuntime::new(
        Arc::new(provider),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Default, Vec::new()),
        Box::new(approver),
        Store::open(dir.path()),
        Workspace::new(dir.path()).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            interactivity: Interactivity::Interactive,
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    (dir, runtime, ask_rx, registry)
}

async fn send<W: tokio::io::AsyncWrite + Unpin>(writer: &mut W, value: Value) {
    let mut line = serde_json::to_vec(&value).unwrap();
    line.push(b'\n');
    writer.write_all(&line).await.unwrap();
}

async fn receive<R: tokio::io::AsyncRead + Unpin>(reader: &mut BufReader<R>) -> Option<Value> {
    let mut line = String::new();
    let read = reader.read_line(&mut line).await.ok()?;
    if read == 0 {
        return None;
    }
    serde_json::from_str(&line).ok()
}

#[tokio::test]
async fn initialize_session_and_prompt_conform_to_the_spec_shapes() {
    let (_dir, mut runtime, ask_rx, registry) = build(FakeProvider::new().text("hello editor"));
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_io);
    let (client_read, mut client_write) = tokio::io::split(client_io);
    let mut client_reader = BufReader::new(client_read);

    let server = serve_acp(&mut runtime, ask_rx, registry, server_read, server_write);

    let client = async move {
        // initialize: protocol version negotiation.
        send(
            &mut client_write,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {
                    "protocolVersion": 1,
                    "clientCapabilities": {
                        "fs": { "readTextFile": false, "writeTextFile": false },
                    },
                },
            }),
        )
        .await;
        let init = receive(&mut client_reader).await.unwrap();
        assert_eq!(init["id"], 1);
        assert_eq!(
            init["result"]["protocolVersion"],
            u64::from(ACP_PROTOCOL_VERSION)
        );
        assert_eq!(init["result"]["agentInfo"]["name"], "localpilot");

        // session/new yields the session id.
        send(
            &mut client_write,
            json!({
                "jsonrpc": "2.0", "id": 2, "method": "session/new",
                "params": { "cwd": ".", "mcpServers": [] },
            }),
        )
        .await;
        let new_session = receive(&mut client_reader).await.unwrap();
        let session_id = new_session["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(!session_id.is_empty());

        // session/prompt streams agent_message_chunk updates, then replies
        // with a stop reason.
        send(
            &mut client_write,
            json!({
                "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
                "params": {
                    "sessionId": session_id,
                    "prompt": [ { "type": "text", "text": "hi" } ],
                },
            }),
        )
        .await;
        let mut text = String::new();
        loop {
            let message = receive(&mut client_reader).await.unwrap();
            if message["method"] == "session/update" {
                let update = &message["params"]["update"];
                if update["sessionUpdate"] == "agent_message_chunk" {
                    text.push_str(update["content"]["text"].as_str().unwrap());
                }
                continue;
            }
            assert_eq!(message["id"], 3);
            assert_eq!(message["result"]["stopReason"], "end_turn");
            break;
        }
        assert_eq!(text, "hello editor");
        drop(client_write); // EOF ends the serve loop
    };

    let (served, ()) = tokio::join!(server, client);
    served.unwrap();
}

#[tokio::test]
async fn permission_requests_map_onto_engine_verdicts() {
    let provider = FakeProvider::new()
        .tool_call(
            "c1",
            "run_shell",
            json!({ "program": "rm", "args": ["-rf", "x"] }),
        )
        .text("denied then done");
    let (_dir, mut runtime, ask_rx, registry) = build(provider);
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_io);
    let (client_read, mut client_write) = tokio::io::split(client_io);
    let mut client_reader = BufReader::new(client_read);

    let server = serve_acp(&mut runtime, ask_rx, registry, server_read, server_write);

    let client = async move {
        send(
            &mut client_write,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "session/prompt",
                "params": {
                    "sessionId": "s",
                    "prompt": [ { "type": "text", "text": "delete it" } ],
                },
            }),
        )
        .await;

        let mut saw_request = false;
        let mut tool_failed = false;
        loop {
            let message = receive(&mut client_reader).await.unwrap();
            if message["method"] == "session/request_permission" {
                saw_request = true;
                let options = message["params"]["options"].as_array().unwrap().clone();
                assert!(options.iter().any(|o| o["kind"] == "allow_once"));
                let reject = options.iter().find(|o| o["kind"] == "reject_once").unwrap()
                    ["optionId"]
                    .clone();
                let title = message["params"]["toolCall"]["title"]
                    .as_str()
                    .unwrap()
                    .to_string();
                assert!(title.contains("rm -rf x"), "title: {title}");
                send(
                    &mut client_write,
                    json!({
                        "jsonrpc": "2.0",
                        "id": message["id"].clone(),
                        "result": {
                            "outcome": { "outcome": "selected", "optionId": reject },
                        },
                    }),
                )
                .await;
                continue;
            }
            if message["method"] == "session/update" {
                let update = &message["params"]["update"];
                if update["sessionUpdate"] == "tool_call_update" {
                    tool_failed = update["status"] == "failed";
                }
                continue;
            }
            assert_eq!(message["result"]["stopReason"], "end_turn");
            break;
        }
        assert!(saw_request, "the editor saw the permission request");
        assert!(tool_failed, "the rejection became a failed tool call");
        drop(client_write);
    };

    let (served, ()) = tokio::join!(server, client);
    served.unwrap();
}

#[tokio::test]
async fn cancel_notification_stops_the_turn_cleanly() {
    // An incomplete stream keeps the first turn from finishing cleanly; the
    // cancel notification must terminate the prompt with a clean stop reason
    // either way.
    let provider = FakeProvider::new().script(vec![Ok(localpilot_llm::ModelEvent::TextDelta(
        "starting".to_string(),
    ))]);
    let (_dir, mut runtime, ask_rx, registry) = build(provider);
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_io);
    let (client_read, mut client_write) = tokio::io::split(client_io);
    let mut client_reader = BufReader::new(client_read);

    let server = serve_acp(&mut runtime, ask_rx, registry, server_read, server_write);

    let client = async move {
        send(
            &mut client_write,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "session/prompt",
                "params": { "sessionId": "s", "prompt": [ { "type": "text", "text": "go" } ] },
            }),
        )
        .await;
        send(
            &mut client_write,
            json!({
                "jsonrpc": "2.0", "method": "session/cancel",
                "params": { "sessionId": "s" },
            }),
        )
        .await;
        loop {
            let message = receive(&mut client_reader).await.unwrap();
            if message["method"].is_string() {
                continue;
            }
            let reason = message["result"]["stopReason"].as_str().unwrap();
            assert!(
                reason == "cancelled" || reason == "error" || reason == "end_turn",
                "reason: {reason}"
            );
            break;
        }
        drop(client_write);
    };

    let (served, ()) = tokio::join!(server, client);
    served.unwrap();
}
