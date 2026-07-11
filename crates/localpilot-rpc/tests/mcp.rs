//! End-to-end MCP-adapter tests over an in-memory duplex transport.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use localpilot_harness::{SessionConfig, SessionRuntime};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_rpc::{serve_mcp, McpServeOptions, RpcApprover, MCP_PROTOCOL_VERSION};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, Workspace};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn request(id: u64, method: &str, params: Value) -> String {
    let mut line =
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string();
    line.push('\n');
    line
}

fn call(id: u64, tool: &str, arguments: Value) -> String {
    request(
        id,
        "tools/call",
        json!({ "name": tool, "arguments": arguments }),
    )
}

async fn next_message<R: tokio::io::AsyncRead + Unpin>(reader: &mut BufReader<R>) -> Option<Value> {
    let mut line = String::new();
    let read = reader.read_line(&mut line).await.ok()?;
    if read == 0 {
        return None;
    }
    serde_json::from_str(&line).ok()
}

/// The structured content of a tool result.
fn structured(message: &Value) -> Value {
    message["result"]["structuredContent"].clone()
}

type Built = (
    tempfile::TempDir,
    SessionRuntime,
    tokio::sync::mpsc::UnboundedReceiver<localpilot_rpc::PendingAsk>,
    localpilot_rpc::AskRegistry,
);

fn build_full(provider: FakeProvider) -> Built {
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

fn options(root: &std::path::Path, approvals: bool) -> McpServeOptions {
    McpServeOptions {
        model: "test-model".to_string(),
        profile: "default".to_string(),
        root: Some(root.to_path_buf()),
        approvals,
    }
}

#[tokio::test]
async fn initialize_negotiates_and_lists_the_tool_catalog() {
    let (dir, mut runtime, ask_rx, registry) = build_full(FakeProvider::new());
    let opts = options(dir.path(), true);
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_io);
    let (client_read, mut client_write) = tokio::io::split(client_io);
    let mut client_reader = BufReader::new(client_read);

    let server = serve_mcp(
        &mut runtime,
        ask_rx,
        registry,
        server_read,
        server_write,
        &opts,
    );

    let client = async move {
        client_write
            .write_all(
                request(
                    1,
                    "initialize",
                    json!({ "protocolVersion": "2025-06-18", "capabilities": {},
                            "clientInfo": { "name": "test", "version": "0" } }),
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let init = next_message(&mut client_reader).await.unwrap();
        assert_eq!(
            init["result"]["protocolVersion"].as_str(),
            Some(MCP_PROTOCOL_VERSION)
        );
        assert_eq!(
            init["result"]["serverInfo"]["name"].as_str(),
            Some("localpilot")
        );

        client_write
            .write_all(request(2, "tools/list", json!({})).as_bytes())
            .await
            .unwrap();
        let list = next_message(&mut client_reader).await.unwrap();
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        assert_eq!(
            names,
            [
                "prompt",
                "events",
                "status",
                "cancel",
                "transcript",
                "reply_permission"
            ]
        );

        // Closing the input stream is the spec's stdio shutdown.
        drop(client_write);
    };

    let (served, ()) = tokio::join!(server, client);
    served.unwrap();
}

#[tokio::test]
async fn prompt_runs_a_turn_and_events_page_the_feed() {
    let (dir, mut runtime, ask_rx, registry) = build_full(FakeProvider::new().text("the answer"));
    let opts = options(dir.path(), true);
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_io);
    let (client_read, mut client_write) = tokio::io::split(client_io);
    let mut client_reader = BufReader::new(client_read);

    let server = serve_mcp(
        &mut runtime,
        ask_rx,
        registry,
        server_read,
        server_write,
        &opts,
    );

    let client = async move {
        client_write
            .write_all(call(1, "prompt", json!({ "text": "go" })).as_bytes())
            .await
            .unwrap();
        let started = next_message(&mut client_reader).await.unwrap();
        assert_eq!(structured(&started)["started"].as_bool(), Some(true));

        let mut text = String::new();
        let mut cursor = 0u64;
        let mut request_id = 2u64;
        'poll: loop {
            client_write
                .write_all(
                    call(
                        request_id,
                        "events",
                        json!({ "cursor": cursor, "wait_ms": 5000 }),
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            request_id += 1;
            let page = next_message(&mut client_reader).await.unwrap();
            let result = structured(&page);
            cursor = result["next_cursor"].as_u64().unwrap();
            assert_eq!(result["dropped"].as_u64(), Some(0));
            for entry in result["events"].as_array().unwrap() {
                let event = &entry["event"];
                match event["type"].as_str().unwrap() {
                    "text_delta" => text.push_str(event["text"].as_str().unwrap()),
                    "stopped" => {
                        assert_eq!(event["reason"].as_str(), Some("done"));
                        break 'poll;
                    }
                    _ => {}
                }
            }
        }
        assert_eq!(text, "the answer");

        drop(client_write);
    };

    let (served, ()) = tokio::join!(server, client);
    served.unwrap();
}

#[tokio::test]
async fn a_denied_ask_becomes_a_model_visible_tool_error() {
    let provider = FakeProvider::new()
        .tool_call(
            "c1",
            "run_shell",
            json!({ "program": "rm", "args": ["-rf", "x"] }),
        )
        .text("could not delete");
    let (dir, mut runtime, ask_rx, registry) = build_full(provider);
    let opts = options(dir.path(), true);
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_io);
    let (client_read, mut client_write) = tokio::io::split(client_io);
    let mut client_reader = BufReader::new(client_read);

    let server = serve_mcp(
        &mut runtime,
        ask_rx,
        registry,
        server_read,
        server_write,
        &opts,
    );

    let client = async move {
        client_write
            .write_all(call(1, "prompt", json!({ "text": "delete it" })).as_bytes())
            .await
            .unwrap();
        let _started = next_message(&mut client_reader).await.unwrap();

        let mut saw_ask = false;
        let mut tool_errored = false;
        let mut cursor = 0u64;
        let mut request_id = 2u64;
        'poll: loop {
            client_write
                .write_all(
                    call(
                        request_id,
                        "events",
                        json!({ "cursor": cursor, "wait_ms": 5000 }),
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            request_id += 1;
            let page = next_message(&mut client_reader).await.unwrap();
            let result = structured(&page);
            cursor = result["next_cursor"].as_u64().unwrap();
            for entry in result["events"].as_array().unwrap() {
                let event = &entry["event"];
                match event["type"].as_str().unwrap() {
                    "permission_ask" => {
                        saw_ask = true;
                        assert_eq!(event["tool"].as_str(), Some("run_shell"));
                        client_write
                            .write_all(
                                call(
                                    request_id,
                                    "reply_permission",
                                    json!({ "ask_id": event["ask_id"], "allow": false }),
                                )
                                .as_bytes(),
                            )
                            .await
                            .unwrap();
                        request_id += 1;
                        let reply = next_message(&mut client_reader).await.unwrap();
                        assert_eq!(structured(&reply)["resolved"].as_bool(), Some(true));
                    }
                    "tool_finished" => {
                        tool_errored = event["is_error"].as_bool().unwrap_or(false);
                    }
                    "stopped" => break 'poll,
                    _ => {}
                }
            }
        }
        assert!(saw_ask, "the ask reached the client as an event");
        assert!(tool_errored, "the denial became a model-visible error");

        drop(client_write);
    };

    let (served, ()) = tokio::join!(server, client);
    served.unwrap();
}

#[tokio::test]
async fn no_approvals_mode_withholds_the_reply_tool() {
    let (dir, mut runtime, ask_rx, registry) = build_full(FakeProvider::new());
    let opts = options(dir.path(), false);
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_io);
    let (client_read, mut client_write) = tokio::io::split(client_io);
    let mut client_reader = BufReader::new(client_read);

    let server = serve_mcp(
        &mut runtime,
        ask_rx,
        registry,
        server_read,
        server_write,
        &opts,
    );

    let client = async move {
        client_write
            .write_all(request(1, "tools/list", json!({})).as_bytes())
            .await
            .unwrap();
        let list = next_message(&mut client_reader).await.unwrap();
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        assert!(!names.contains(&"reply_permission"));

        // Calling the withheld tool is an unknown-tool protocol error, so a
        // client cannot answer asks in watch-and-steer mode.
        client_write
            .write_all(
                call(
                    2,
                    "reply_permission",
                    json!({ "ask_id": "x", "allow": true }),
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let refused = next_message(&mut client_reader).await.unwrap();
        assert_eq!(refused["error"]["code"].as_i64(), Some(-32602));

        drop(client_write);
    };

    let (served, ()) = tokio::join!(server, client);
    served.unwrap();
}

#[tokio::test]
async fn an_idle_bounded_wait_returns_an_empty_page_at_its_deadline() {
    let (dir, mut runtime, ask_rx, registry) = build_full(FakeProvider::new());
    let opts = options(dir.path(), true);
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_io);
    let (client_read, mut client_write) = tokio::io::split(client_io);
    let mut client_reader = BufReader::new(client_read);

    let server = serve_mcp(
        &mut runtime,
        ask_rx,
        registry,
        server_read,
        server_write,
        &opts,
    );

    let client = async move {
        client_write
            .write_all(call(1, "events", json!({ "cursor": 0, "wait_ms": 50 })).as_bytes())
            .await
            .unwrap();
        let page = next_message(&mut client_reader).await.unwrap();
        let result = structured(&page);
        assert_eq!(result["events"].as_array().map(Vec::len), Some(0));
        assert_eq!(result["next_cursor"].as_u64(), Some(0));

        drop(client_write);
    };

    let (served, ()) = tokio::join!(server, client);
    served.unwrap();
}

#[tokio::test]
async fn status_and_unknown_tools_behave() {
    let (dir, mut runtime, ask_rx, registry) = build_full(FakeProvider::new());
    let opts = options(dir.path(), true);
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_io);
    let (client_read, mut client_write) = tokio::io::split(client_io);
    let mut client_reader = BufReader::new(client_read);

    let server = serve_mcp(
        &mut runtime,
        ask_rx,
        registry,
        server_read,
        server_write,
        &opts,
    );

    let client = async move {
        client_write
            .write_all(call(1, "status", json!({})).as_bytes())
            .await
            .unwrap();
        let status = next_message(&mut client_reader).await.unwrap();
        let result = structured(&status);
        assert_eq!(result["model"].as_str(), Some("test-model"));
        assert_eq!(result["profile"].as_str(), Some("default"));
        assert_eq!(result["busy"].as_bool(), Some(false));

        client_write
            .write_all(call(2, "no_such_tool", json!({})).as_bytes())
            .await
            .unwrap();
        let unknown = next_message(&mut client_reader).await.unwrap();
        assert_eq!(unknown["error"]["code"].as_i64(), Some(-32602));

        // A cancel with no running turn is a tool-level error, not a crash.
        client_write
            .write_all(call(3, "cancel", json!({})).as_bytes())
            .await
            .unwrap();
        let cancel = next_message(&mut client_reader).await.unwrap();
        assert_eq!(cancel["result"]["isError"].as_bool(), Some(true));

        drop(client_write);
    };

    let (served, ()) = tokio::join!(server, client);
    served.unwrap();
}
