//! MCP resources cross the real stdio JSON-RPC transport in both directions.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use localpilot_core::Secret;
use localpilot_mcp::{McpClient, ResolvedEnvEntry, ServerEnvironment, StdioTransport};
use serde_json::{json, Value};

const CHILD_MARKER: &str = "LOCALPILOT_MCP_RESOURCE_PROBE";
const RESOURCE_URI: &str = "file:///guide.txt";

#[test]
fn resource_probe_child() {
    if std::env::var(CHILD_MARKER).is_err() {
        return;
    }
    use std::io::{BufRead, Write};

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines().map_while(Result::ok) {
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let result = match method {
            "initialize" => json!({
                "protocolVersion": "2025-06-18",
                "capabilities": { "resources": {} },
                "serverInfo": { "name": "resource-probe", "version": "1" }
            }),
            "resources/list" => json!({
                "resources": [{
                    "uri": RESOURCE_URI,
                    "name": "guide",
                    "description": "Project guide",
                    "mimeType": "text/plain"
                }]
            }),
            "resources/read" => {
                assert_eq!(
                    request["params"]["uri"].as_str(),
                    Some(RESOURCE_URI),
                    "the discovered URI must be sent back unchanged"
                );
                json!({
                    "contents": [{
                        "uri": RESOURCE_URI,
                        "mimeType": "text/plain",
                        "text": "hello from an MCP resource"
                    }]
                })
            }
            other => panic!("unexpected MCP method: {other}"),
        };
        let response = json!({ "jsonrpc": "2.0", "id": id, "result": result });
        let _ = writeln!(stdout, "\n{response}");
        let _ = stdout.flush();
    }
}

fn spawn() -> StdioTransport {
    let executable = std::env::current_exe().unwrap();
    let args = [
        "resource_probe_child".to_string(),
        "--exact".to_string(),
        "--nocapture".to_string(),
        "--test-threads=1".to_string(),
    ];
    let environment = ServerEnvironment::new(vec![ResolvedEnvEntry {
        name: CHILD_MARKER.to_string(),
        value: Secret::new("1"),
        sensitive: false,
    }]);
    StdioTransport::spawn(executable.to_str().unwrap(), &args, &environment).unwrap()
}

#[tokio::test]
async fn lists_then_reads_a_resource_over_stdio() {
    let client = McpClient::new(Arc::new(spawn()));

    let status = client.initialize().await.unwrap();
    assert!(status.supports_resources);
    assert!(!status.supports_tools);

    let page = client.list_resources(None).await.unwrap();
    assert_eq!(page.resources.len(), 1);
    assert_eq!(page.resources[0].uri, RESOURCE_URI);
    assert_eq!(page.resources[0].mime_type.as_deref(), Some("text/plain"));

    let content = client.read_resource(&page.resources[0].uri).await.unwrap();
    assert_eq!(content, "hello from an MCP resource");
}
