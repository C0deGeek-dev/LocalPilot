//! A minimal async Chrome DevTools Protocol client over a local WebSocket.
//!
//! CDP is an official, documented protocol; this is an original, dependency-light
//! client for it — a background reader task demultiplexes the single WebSocket
//! into command *responses* (matched to their request `id`) and *events* (which
//! carry a `method` and no `id`). [`CdpClient::call`] issues one command and
//! awaits its response; events are delivered on the channel returned by
//! [`connect`], so the renderer can answer paused requests and watch page
//! lifecycle while a command is in flight.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::RenderError;

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

/// A connected CDP client. Commands go out through the shared sink; responses
/// come back through per-`id` oneshot channels the reader task fulfils.
pub(crate) struct CdpClient {
    next_id: AtomicU64,
    pending: Pending,
    sink: Mutex<SplitSink<Ws, Message>>,
    reader: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for CdpClient {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.reader.try_lock() {
            if let Some(handle) = guard.take() {
                handle.abort();
            }
        }
    }
}

/// Connect to a CDP WebSocket endpoint. Returns the client and the stream of
/// protocol events (every message carrying a `method`), spawning the background
/// reader that routes responses and events.
pub(crate) async fn connect(
    ws_url: &str,
) -> Result<(CdpClient, mpsc::UnboundedReceiver<Value>), RenderError> {
    let (stream, _response) = connect_async(ws_url)
        .await
        .map_err(|error| RenderError::Connect(error.to_string()))?;
    let (sink, mut source) = stream.split();
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let (event_tx, event_rx) = mpsc::unbounded_channel();

    let reader_pending = Arc::clone(&pending);
    let reader = tokio::spawn(async move {
        while let Some(message) = source.next().await {
            let text = match message {
                Ok(Message::Text(text)) => text.to_string(),
                Ok(Message::Binary(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(_) => continue,
            };
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if let Some(id) = value.get("id").and_then(Value::as_u64) {
                if let Some(sender) = reader_pending.lock().await.remove(&id) {
                    let _ = sender.send(value);
                }
            } else if value.get("method").is_some() {
                // A protocol event: forward it; a dropped receiver just ends the
                // render, so a send failure is not fatal here.
                let _ = event_tx.send(value);
            }
        }
    });

    Ok((
        CdpClient {
            next_id: AtomicU64::new(1),
            pending,
            sink: Mutex::new(sink),
            reader: Mutex::new(Some(reader)),
        },
        event_rx,
    ))
}

impl CdpClient {
    /// Issue one CDP command and await its result. `session` scopes the command
    /// to an attached target (flattened session); `None` targets the browser
    /// endpoint. Returns the command's `result` object, or a protocol error.
    pub(crate) async fn call(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> Result<Value, RenderError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let mut message = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        self.sink
            .lock()
            .await
            .send(Message::Text(message.to_string()))
            .await
            .map_err(|error| RenderError::Protocol(error.to_string()))?;

        let response = rx
            .await
            .map_err(|_| RenderError::Protocol("connection closed before response".to_string()))?;
        if let Some(error) = response.get("error") {
            return Err(RenderError::Protocol(error.to_string()));
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Fire-and-forget a command whose result we do not need (e.g. answering a
    /// paused request), so the caller does not block on the round-trip.
    pub(crate) async fn send(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> Result<(), RenderError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut message = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        self.sink
            .lock()
            .await
            .send(Message::Text(message.to_string()))
            .await
            .map_err(|error| RenderError::Protocol(error.to_string()))
    }
}
