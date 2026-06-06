//! MCP tools go through the same permission engine and redaction as builtin
//! tools: an MCP write is denied exactly like a builtin write, and MCP output is
//! redacted.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use serde_json::json;
use localpilot_core::{ToolCall, ToolUseId};
use localpilot_mcp::{McpTool, McpToolDescriptor, ScriptedTransport};
use localpilot_sandbox::{
    Effect, Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace,
};
use localpilot_tools::{ToolContext, ToolRegistry};

fn descriptor(name: &str) -> McpToolDescriptor {
    McpToolDescriptor {
        name: name.to_string(),
        description: "an mcp tool".to_string(),
        input_schema: json!({ "type": "object" }),
    }
}

#[tokio::test]
async fn an_mcp_write_is_denied_like_a_builtin_write() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();

    // The MCP tool declares an out-of-workspace write effect.
    let transport = Arc::new(ScriptedTransport::new().with(
        "tools/call",
        json!({ "content": [{ "type": "text", "text": "ok" }] }),
    ));
    let tool = McpTool::new(
        &descriptor("mcp_write"),
        vec![Effect::WritePath {
            inside_workspace: false,
            overwrite: false,
        }],
        transport,
    );
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(tool));

    let call = ToolCall::new(ToolUseId::from("c1"), "mcp_write", json!({}));
    let ctx = ToolContext {
        workspace: &ws,
        interactivity: Interactivity::NonInteractive,
        trusted: true,
    };
    let result = registry
        .dispatch(
            &call,
            &ctx,
            &PermissionEngine::new(Profile::Default, Vec::new()),
            &ScriptedApprover::always(),
        )
        .await;

    assert!(result.is_error);
    assert!(result.output.contains("permission denied"));
}

#[tokio::test]
async fn mcp_tool_output_is_redacted() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();

    let secret = "sk-abcdefghijklmnopqrstuvwxyz0123";
    let transport = Arc::new(ScriptedTransport::new().with(
        "tools/call",
        json!({ "content": [{ "type": "text", "text": format!("key {secret}") }] }),
    ));
    // A read-only effect so the call is allowed.
    let tool = McpTool::new(
        &descriptor("mcp_read"),
        vec![Effect::ReadPath {
            inside_workspace: true,
            secret_like: false,
        }],
        transport,
    );
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(tool));

    let call = ToolCall::new(ToolUseId::from("c2"), "mcp_read", json!({}));
    let ctx = ToolContext {
        workspace: &ws,
        interactivity: Interactivity::NonInteractive,
        trusted: true,
    };
    let result = registry
        .dispatch(
            &call,
            &ctx,
            &PermissionEngine::new(Profile::Default, Vec::new()),
            &ScriptedApprover::always(),
        )
        .await;

    assert!(!result.is_error, "{}", result.output);
    assert!(
        !result.output.contains(secret),
        "secret leaked: {}",
        result.output
    );
    assert!(result.output.contains("[REDACTED]"));
}

#[tokio::test]
async fn repeated_mcp_registry_rebuilds_preserve_dynamic_metadata_and_routing() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let descriptor = McpToolDescriptor {
        name: "dynamic_echo".to_string(),
        description: "echo through mcp".to_string(),
        input_schema: json!({ "type": "object", "properties": { "text": { "type": "string" } } }),
    };

    for expected in ["first", "second"] {
        let transport = Arc::new(ScriptedTransport::new().with(
            "tools/call",
            json!({ "content": [{ "type": "text", "text": expected }] }),
        ));
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(McpTool::new(
            &descriptor,
            vec![Effect::ReadPath {
                inside_workspace: true,
                secret_like: false,
            }],
            transport,
        )));

        let specs = registry.specs();
        assert_eq!(specs[0].0, "dynamic_echo");
        assert_eq!(specs[0].1, "echo through mcp");

        let call = ToolCall::new(
            ToolUseId::from(format!("c_{expected}").as_str()),
            "dynamic_echo",
            json!({ "text": expected }),
        );
        let ctx = ToolContext {
            workspace: &ws,
            interactivity: Interactivity::NonInteractive,
            trusted: true,
        };
        let result = registry
            .dispatch(
                &call,
                &ctx,
                &PermissionEngine::new(Profile::Default, Vec::new()),
                &ScriptedApprover::always(),
            )
            .await;

        assert!(!result.is_error, "{}", result.output);
        assert!(result.output.contains(expected), "{}", result.output);
    }
}
