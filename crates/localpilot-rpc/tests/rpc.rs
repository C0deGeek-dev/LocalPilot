//! End-to-end serve-loop tests over an in-memory duplex transport.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use localpilot_harness::{SessionConfig, SessionRuntime};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_rpc::{
    serve, ClientCommand, ClientRecord, InputDisposition, RpcApprover, ServeContext, ServerEvent,
    ServerRecord, RPC_PROTOCOL_VERSION,
};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, Workspace};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn record(id: Option<&str>, command: ClientCommand) -> String {
    let mut line = serde_json::to_string(&ClientRecord {
        v: RPC_PROTOCOL_VERSION,
        id: id.map(str::to_string),
        command,
    })
    .unwrap();
    line.push('\n');
    line
}

async fn next_event<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Option<ServerRecord> {
    let mut line = String::new();
    let read = reader.read_line(&mut line).await.ok()?;
    if read == 0 {
        return None;
    }
    serde_json::from_str(&line).ok()
}

type Built = (
    tempfile::TempDir,
    SessionRuntime,
    ServeContext,
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
    let context = ServeContext {
        model: "test-model".to_string(),
        profile: "default".to_string(),
        root: Some(dir.path().to_path_buf()),
    };
    (dir, runtime, context, ask_rx, registry)
}

#[tokio::test]
async fn hello_prompt_and_shutdown_round_trip() {
    let (_dir, mut runtime, context, ask_rx, registry) =
        build_full(FakeProvider::new().text("the answer"));
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_io);
    let (client_read, mut client_write) = tokio::io::split(client_io);
    let mut client_reader = BufReader::new(client_read);

    let server = serve(
        &mut runtime,
        ask_rx,
        registry,
        server_read,
        server_write,
        &context,
    );

    let client = async move {
        client_write
            .write_all(record(Some("1"), ClientCommand::Hello).as_bytes())
            .await
            .unwrap();
        let hello = next_event(&mut client_reader).await.unwrap();
        assert_eq!(hello.id.as_deref(), Some("1"));
        assert!(matches!(
            hello.event,
            ServerEvent::Hello { protocol_version, .. } if protocol_version == RPC_PROTOCOL_VERSION
        ));

        client_write
            .write_all(
                record(
                    None,
                    ClientCommand::Prompt {
                        text: "go".to_string(),
                        disposition: InputDisposition::Immediate,
                    },
                )
                .as_bytes(),
            )
            .await
            .unwrap();

        let mut text = String::new();
        loop {
            let event = next_event(&mut client_reader).await.unwrap();
            match event.event {
                ServerEvent::TextDelta { text: delta } => text.push_str(&delta),
                ServerEvent::Stopped { reason } => {
                    assert_eq!(reason, "done");
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(text, "the answer");

        client_write
            .write_all(record(Some("2"), ClientCommand::Shutdown).as_bytes())
            .await
            .unwrap();
        let closed = next_event(&mut client_reader).await.unwrap();
        assert!(matches!(closed.event, ServerEvent::Closed));
    };

    let (served, ()) = tokio::join!(server, client);
    served.unwrap();
}

#[tokio::test]
async fn permission_asks_flow_over_the_wire_and_denial_is_an_error_result() {
    // A destructive command asks; the client denies; the tool result is an
    // error and the turn still completes.
    let provider = FakeProvider::new()
        .tool_call(
            "c1",
            "run_shell",
            json!({ "program": "rm", "args": ["-rf", "x"] }),
        )
        .text("could not delete");
    let (_dir, mut runtime, context, ask_rx, registry) = build_full(provider);
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_io);
    let (client_read, mut client_write) = tokio::io::split(client_io);
    let mut client_reader = BufReader::new(client_read);

    let server = serve(
        &mut runtime,
        ask_rx,
        registry,
        server_read,
        server_write,
        &context,
    );

    let client = async move {
        client_write
            .write_all(
                record(
                    None,
                    ClientCommand::Prompt {
                        text: "delete it".to_string(),
                        disposition: InputDisposition::Immediate,
                    },
                )
                .as_bytes(),
            )
            .await
            .unwrap();

        let mut saw_ask = false;
        let mut tool_errored = false;
        loop {
            let event = next_event(&mut client_reader).await.unwrap();
            match event.event {
                ServerEvent::PermissionAsk {
                    ask_id,
                    tool,
                    detail,
                    ..
                } => {
                    saw_ask = true;
                    assert_eq!(tool, "run_shell");
                    assert_eq!(detail, "rm -rf x");
                    client_write
                        .write_all(
                            record(
                                None,
                                ClientCommand::PermissionReply {
                                    ask_id,
                                    allow: false,
                                },
                            )
                            .as_bytes(),
                        )
                        .await
                        .unwrap();
                }
                ServerEvent::ToolFinished { is_error, .. } => tool_errored = is_error,
                ServerEvent::Stopped { reason } => {
                    assert_eq!(reason, "done");
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_ask, "the ask reached the client");
        assert!(tool_errored, "the denial became a model-visible error");

        client_write
            .write_all(record(None, ClientCommand::Shutdown).as_bytes())
            .await
            .unwrap();
        let _ = next_event(&mut client_reader).await;
    };

    let (served, ()) = tokio::join!(server, client);
    served.unwrap();
}

#[tokio::test]
async fn follow_up_input_runs_after_the_current_turn() {
    let provider = FakeProvider::new().text("first").text("second");
    let (_dir, mut runtime, context, ask_rx, registry) = build_full(provider);
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_io);
    let (client_read, mut client_write) = tokio::io::split(client_io);
    let mut client_reader = BufReader::new(client_read);

    let server = serve(
        &mut runtime,
        ask_rx,
        registry,
        server_read,
        server_write,
        &context,
    );

    let client = async move {
        // Send both prompts back to back; whether the second arrives mid-turn
        // (queued) or at idle (runs directly), two turns must complete in
        // order.
        client_write
            .write_all(
                record(
                    None,
                    ClientCommand::Prompt {
                        text: "one".to_string(),
                        disposition: InputDisposition::Immediate,
                    },
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        client_write
            .write_all(
                record(
                    None,
                    ClientCommand::Prompt {
                        text: "two".to_string(),
                        disposition: InputDisposition::FollowUp,
                    },
                )
                .as_bytes(),
            )
            .await
            .unwrap();

        let mut stops = 0;
        let mut text = String::new();
        while stops < 2 {
            let event = next_event(&mut client_reader).await.unwrap();
            match event.event {
                ServerEvent::TextDelta { text: delta } => text.push_str(&delta),
                ServerEvent::Stopped { .. } => stops += 1,
                _ => {}
            }
        }
        assert_eq!(text, "firstsecond");

        client_write
            .write_all(record(None, ClientCommand::Shutdown).as_bytes())
            .await
            .unwrap();
        let _ = next_event(&mut client_reader).await;
    };

    let (served, ()) = tokio::join!(server, client);
    served.unwrap();
}

#[tokio::test]
async fn status_reports_session_and_harness_state() {
    let (_dir, mut runtime, context, ask_rx, registry) = build_full(FakeProvider::new());
    std::fs::write(
        context.root.as_deref().unwrap().join("PROGRESS.md"),
        "# Progress: x\nBranch: b\n\n## Steps\n\n- [x] 1. Done step\n- [ ] 2. Next step\n",
    )
    .unwrap();
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_io);
    let (client_read, mut client_write) = tokio::io::split(client_io);
    let mut client_reader = BufReader::new(client_read);

    let server = serve(
        &mut runtime,
        ask_rx,
        registry,
        server_read,
        server_write,
        &context,
    );

    let client = async move {
        client_write
            .write_all(record(Some("s"), ClientCommand::Status).as_bytes())
            .await
            .unwrap();
        let status = next_event(&mut client_reader).await.unwrap();
        match status.event {
            ServerEvent::Status {
                model,
                profile,
                busy,
                next_step,
                ..
            } => {
                assert_eq!(model, "test-model");
                assert_eq!(profile, "default");
                assert!(!busy);
                assert_eq!(next_step.as_deref(), Some("2. Next step"));
            }
            other => panic!("expected status, got {other:?}"),
        }

        client_write
            .write_all(record(None, ClientCommand::Shutdown).as_bytes())
            .await
            .unwrap();
        let _ = next_event(&mut client_reader).await;
    };

    let (served, ()) = tokio::join!(server, client);
    served.unwrap();
}

#[tokio::test]
async fn a_newer_protocol_version_is_a_typed_error() {
    let (_dir, mut runtime, context, ask_rx, registry) = build_full(FakeProvider::new());
    let (client_io, server_io) = tokio::io::duplex(8 * 1024);
    let (server_read, server_write) = tokio::io::split(server_io);
    let (client_read, mut client_write) = tokio::io::split(client_io);
    let mut client_reader = BufReader::new(client_read);

    let server = serve(
        &mut runtime,
        ask_rx,
        registry,
        server_read,
        server_write,
        &context,
    );

    let client = async move {
        client_write
            .write_all(b"{\"v\":99,\"command\":{\"type\":\"hello\"}}\n")
            .await
            .unwrap();
        let error = next_event(&mut client_reader).await.unwrap();
        assert!(matches!(
            error.event,
            ServerEvent::Error { message } if message.contains("protocol version")
        ));
        client_write
            .write_all(record(None, ClientCommand::Shutdown).as_bytes())
            .await
            .unwrap();
        let _ = next_event(&mut client_reader).await;
    };

    let (served, ()) = tokio::join!(server, client);
    served.unwrap();
}
