//! MCP transport abstraction.
//!
//! A real transport speaks JSON-RPC over a server process's stdio; tests use a
//! scripted transport so the client can be exercised offline.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex as AsyncMutex;

use crate::error::McpError;

/// Windows `CREATE_NO_WINDOW`: give the server its own invisible console
/// instead of attaching it to the interactive TUI's (see `StdioTransport::spawn`).
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A request/response transport to an MCP server.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Send a JSON-RPC method call and await the result value.
    ///
    /// # Errors
    /// Returns [`McpError::Transport`] or [`McpError::Protocol`] on failure.
    async fn call(&self, method: &str, params: Value) -> Result<Value, McpError>;

    /// Send a JSON-RPC notification: a request with no `id` for which no
    /// response is awaited. Used for `notifications/initialized`.
    ///
    /// The default is a no-op, which suits transports (e.g. the scripted test
    /// transport) that have nothing to notify.
    ///
    /// # Errors
    /// Returns [`McpError::Transport`] or [`McpError::Protocol`] on failure.
    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let _ = (method, params);
        Ok(())
    }
}

/// A live transport that speaks newline-delimited JSON-RPC over an MCP server
/// process's stdin/stdout. Calls are serialized: each `call` writes one request
/// and reads responses until the matching id, skipping notifications.
pub struct StdioTransport {
    inner: AsyncMutex<StdioInner>,
    next_id: AtomicU64,
}

struct StdioInner {
    // Owned so the server process lives as long as the transport; `kill_on_drop`
    // tears it down when the transport is dropped.
    #[allow(dead_code)]
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl StdioTransport {
    /// Spawn `command` with `args` and connect to it over stdio.
    ///
    /// # Errors
    /// Returns [`McpError::Transport`] if the process cannot be spawned or its
    /// stdio cannot be captured.
    pub fn spawn(command: &str, args: &[String]) -> Result<Self, McpError> {
        let mut builder = Command::new(command);
        builder
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        // An MCP server lives for the whole session next to the interactive
        // TUI. Isolate it from the user's terminal: on Windows a shared
        // console would let it read `CONIN$` (stealing keystrokes) or re-cook
        // the shared console mode; on Unix a new process group means a direct
        // `/dev/tty` read gets SIGTTIN instead of the TUI's input.
        #[cfg(windows)]
        builder.creation_flags(CREATE_NO_WINDOW);
        #[cfg(unix)]
        builder.process_group(0);
        let mut child = builder
            .spawn()
            .map_err(|e| McpError::Transport(format!("spawn {command}: {e}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Transport("server stdin unavailable".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Transport("server stdout unavailable".to_string()))?;
        Ok(Self {
            inner: AsyncMutex::new(StdioInner {
                child,
                stdin,
                stdout: BufReader::new(stdout),
            }),
            next_id: AtomicU64::new(1),
        })
    }
}

#[async_trait]
impl Transport for StdioTransport {
    async fn call(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut line =
            serde_json::to_string(&request).map_err(|e| McpError::Protocol(e.to_string()))?;
        line.push('\n');

        let mut inner = self.inner.lock().await;
        inner
            .stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| McpError::Transport(e.to_string()))?;
        inner
            .stdin
            .flush()
            .await
            .map_err(|e| McpError::Transport(e.to_string()))?;

        loop {
            let mut response = String::new();
            let read = inner
                .stdout
                .read_line(&mut response)
                .await
                .map_err(|e| McpError::Transport(e.to_string()))?;
            if read == 0 {
                return Err(McpError::Transport(
                    "server closed the connection".to_string(),
                ));
            }
            let trimmed = response.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
                // A non-JSON line (e.g. server logging) is not our response.
                continue;
            };
            // Skip notifications and responses to other in-flight ids.
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = value.get("error") {
                return Err(McpError::Protocol(error.to_string()));
            }
            return Ok(value.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        // A notification carries no `id` and expects no response.
        let request = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let mut line =
            serde_json::to_string(&request).map_err(|e| McpError::Protocol(e.to_string()))?;
        line.push('\n');

        let mut inner = self.inner.lock().await;
        inner
            .stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| McpError::Transport(e.to_string()))?;
        inner
            .stdin
            .flush()
            .await
            .map_err(|e| McpError::Transport(e.to_string()))?;
        Ok(())
    }
}

/// A scripted transport returning canned results per method, for tests.
#[derive(Default)]
pub struct ScriptedTransport {
    responses: Mutex<HashMap<String, Value>>,
    calls: Mutex<Vec<(String, Value)>>,
}

impl ScriptedTransport {
    /// An empty scripted transport.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Script a result for a method.
    #[must_use]
    pub fn with(self, method: &str, result: Value) -> Self {
        if let Ok(mut responses) = self.responses.lock() {
            responses.insert(method.to_string(), result);
        }
        self
    }

    /// Calls received so far, in order, with their JSON parameters.
    #[must_use]
    pub fn calls(&self) -> Vec<(String, Value)> {
        self.calls
            .lock()
            .map(|calls| calls.clone())
            .unwrap_or_default()
    }
}

#[async_trait]
impl Transport for ScriptedTransport {
    async fn call(&self, method: &str, params: Value) -> Result<Value, McpError> {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push((method.to_string(), params));
        }
        self.responses
            .lock()
            .ok()
            .and_then(|r| r.get(method).cloned())
            .ok_or_else(|| McpError::Protocol(format!("no scripted response for {method}")))
    }
}
