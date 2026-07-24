//! A bounded, egress-gated browser renderer for LocalPilot research.
//!
//! Some documentation pages deliver their real content only after JavaScript
//! runs; static HTTP extraction cannot recover it (LocalHub#37). This crate
//! drives a discovered system Chromium/Chrome/Edge over the Chrome DevTools
//! Protocol to render such a page and return its post-JavaScript content — but
//! strictly inside the research egress boundary: every browser request
//! (navigation, redirect, subresource, frame) is gated through the caller's
//! [`RenderGate`] before it leaves the machine, the browser profile is ephemeral
//! and cookie-less, and the whole render is time-bounded.
//!
//! The crate is optional: `localpilot-cli` depends on it only under the
//! `render-browser` feature. A build without it has no renderer and records
//! `RendererUnavailable` for a page that needed rendering.

#![forbid(unsafe_code)]

mod browser;
mod cdp;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};

use localpilot_research::{
    RenderFailure, RenderGate, RenderRequest, RenderedDoc, RenderedFrame, Renderer,
};

use browser::Browser;
use cdp::CdpClient;

/// Internal errors mapped to a [`RenderFailure`] for the caller.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RenderError {
    #[error("no chromium-family browser found")]
    NoBrowser,
    #[error("browser launch failed: {0}")]
    Launch(String),
    #[error("devtools endpoint did not come up in time")]
    DevToolsTimeout,
    #[error("cdp connect failed: {0}")]
    Connect(String),
    #[error("cdp protocol error: {0}")]
    Protocol(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl RenderError {
    fn failure(&self) -> RenderFailure {
        match self {
            Self::NoBrowser => RenderFailure::Unavailable,
            other => RenderFailure::Browser(other.to_string()),
        }
    }
}

/// A renderer backed by a headless system browser over CDP.
#[derive(Debug, Default, Clone, Copy)]
pub struct ChromiumRenderer;

impl ChromiumRenderer {
    /// Construct a renderer. Cheap: no browser is launched until [`render`] runs.
    ///
    /// [`render`]: Renderer::render
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Whether a usable browser can be found on this machine, so the caller can
    /// avoid signalling render-availability when none exists.
    #[must_use]
    pub fn available() -> bool {
        Browser::discover().is_some()
    }
}

#[async_trait]
impl Renderer for ChromiumRenderer {
    async fn render(
        &self,
        request: &RenderRequest,
        gate: &dyn RenderGate,
    ) -> Result<RenderedDoc, RenderFailure> {
        // One hard wall-clock bound over the whole render (launch + navigate +
        // settle + extract), so a hung page or browser can never stall a run.
        match tokio::time::timeout(request.bounds.timeout, render_page(request, gate)).await {
            Ok(Ok(doc)) => Ok(doc),
            Ok(Err(error)) => Err(error.failure()),
            Err(_) => Err(RenderFailure::Timeout),
        }
    }
}

/// Launch, navigate, settle, and extract the rendered main document. Every
/// browser request is gated; blocked requests are counted so an incomplete DOM
/// is reported, not presented as complete.
async fn render_page(
    request: &RenderRequest,
    gate: &dyn RenderGate,
) -> Result<RenderedDoc, RenderError> {
    let browser = Browser::launch(request.bounds.timeout).await?;
    let (client, mut events) = cdp::connect(browser.ws_url()).await?;

    // Attach to a fresh page target with a flattened session, then enable the
    // domains we drive: Page (lifecycle), Runtime (extraction), and Fetch
    // (per-request egress gating, before the first navigation).
    let target = client
        .call("Target.createTarget", json!({ "url": "about:blank" }), None)
        .await?;
    let target_id = string_field(&target, "targetId")?;
    let attach = client
        .call(
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
            None,
        )
        .await?;
    let session = string_field(&attach, "sessionId")?;
    let session = Some(session.as_str());

    client.call("Page.enable", json!({}), session).await?;
    client.call("Runtime.enable", json!({}), session).await?;
    client
        .call(
            "Fetch.enable",
            json!({ "patterns": [{ "urlPattern": "*" }] }),
            session,
        )
        .await?;

    let blocked = Arc::new(AtomicUsize::new(0));

    // Fire the navigation without awaiting its response: with Fetch
    // interception the navigation request pauses *before* `Page.navigate`
    // resolves, so awaiting it here would deadlock against the pump that
    // continues the paused request. The pump drives the rest.
    client
        .send("Page.navigate", json!({ "url": request.url }), session)
        .await?;

    pump_until_settled(
        &client,
        session,
        &mut events,
        gate,
        &blocked,
        request.bounds.settle,
    )
    .await?;

    // Extract the rendered DOM as HTML for the caller to reduce and admit.
    let evaluated = client
        .call(
            "Runtime.evaluate",
            json!({
                "expression": "document.documentElement ? document.documentElement.outerHTML : ''",
                "returnByValue": true,
            }),
            session,
        )
        .await?;
    let html = evaluated
        .get("result")
        .and_then(|result| result.get("value"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    // Extract accessible child frames (same-origin and `srcdoc`) — their
    // documents are separate from the main DOM, so a documentation iframe's
    // content is otherwise lost. Their network already passed the gate through
    // the main session's Fetch interception.
    let frames = extract_frames(&client, session, request.bounds.max_frames)
        .await
        .unwrap_or_default();

    browser.close().await;

    Ok(RenderedDoc {
        html,
        frames,
        blocked: blocked.load(Ordering::Relaxed),
    })
}

/// Extract the post-JavaScript HTML of each accessible child frame — same-origin
/// frames and inline `srcdoc` frames, whose `contentDocument` is reachable from
/// the main session. Cross-origin frames (whose `contentDocument` is null) are
/// recovered by the caller's gated HTTP path instead. Bounded by `max_frames`.
async fn extract_frames(
    client: &CdpClient,
    session: Option<&str>,
    max_frames: usize,
) -> Result<Vec<RenderedFrame>, RenderError> {
    // Runs in the main session's same-origin context; a cross-origin
    // `contentDocument` access throws and is skipped.
    let expression = format!(
        "(function() {{ var out = []; var frames = document.querySelectorAll('iframe'); \
         for (var i = 0; i < frames.length && out.length < {max_frames}; i++) {{ \
           try {{ var doc = frames[i].contentDocument; \
             if (doc && doc.documentElement) {{ \
               out.push({{ url: frames[i].getAttribute('src') || null, \
                           html: doc.documentElement.outerHTML }}); }} \
           }} catch (e) {{}} }} \
         return JSON.stringify(out); }})()"
    );
    let evaluated = client
        .call(
            "Runtime.evaluate",
            json!({ "expression": expression, "returnByValue": true }),
            session,
        )
        .await?;
    let serialized = evaluated
        .get("result")
        .and_then(|result| result.get("value"))
        .and_then(Value::as_str)
        .unwrap_or("[]");
    let parsed: Value = serde_json::from_str(serialized).unwrap_or(Value::Array(Vec::new()));
    let mut frames = Vec::new();
    if let Some(array) = parsed.as_array() {
        for item in array {
            let html = item
                .get("html")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if html.trim().is_empty() {
                continue;
            }
            let url = item.get("url").and_then(Value::as_str).map(str::to_string);
            frames.push(RenderedFrame { url, html });
        }
    }
    Ok(frames)
}

/// Drive the event loop from just after navigation through a bounded settle
/// window: gate every `Fetch.requestPaused`, and once the load event fires, keep
/// answering paused requests for the settle duration so late `fetch()`-driven
/// content lands before extraction. Bounded: never an indefinite network-idle
/// wait.
async fn pump_until_settled(
    client: &CdpClient,
    session: Option<&str>,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<Value>,
    gate: &dyn RenderGate,
    blocked: &Arc<AtomicUsize>,
    settle: Duration,
) -> Result<(), RenderError> {
    // Phase 1: wait for the load event, gating requests as they pause. The outer
    // render timeout bounds this if the load never fires.
    loop {
        let Some(event) = events.recv().await else {
            return Ok(()); // connection ended; extract whatever rendered
        };
        match event.get("method").and_then(Value::as_str) {
            Some("Fetch.requestPaused") => {
                handle_paused(client, session, &event, gate, blocked).await;
            }
            Some("Page.loadEventFired") => break,
            _ => {}
        }
    }

    // Phase 2: a bounded settle window; keep the gate answering so a page that
    // populates itself via a delayed API call is captured, without waiting
    // indefinitely for network idle.
    let settle_end = Instant::now() + settle;
    loop {
        let remaining = settle_end.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        tokio::select! {
            () = tokio::time::sleep(remaining) => return Ok(()),
            maybe = events.recv() => {
                let Some(event) = maybe else { return Ok(()); };
                if event.get("method").and_then(Value::as_str) == Some("Fetch.requestPaused") {
                    handle_paused(client, session, &event, gate, blocked).await;
                }
            }
        }
    }
}

/// Answer one paused browser request: continue it when the gate allows the URL,
/// otherwise fail it before it leaves the machine and count the block.
async fn handle_paused(
    client: &CdpClient,
    session: Option<&str>,
    event: &Value,
    gate: &dyn RenderGate,
    blocked: &Arc<AtomicUsize>,
) {
    let params = event.get("params");
    let request_id = params
        .and_then(|params| params.get("requestId"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let url = params
        .and_then(|params| params.get("request"))
        .and_then(|request| request.get("url"))
        .and_then(Value::as_str)
        .unwrap_or_default();

    if gate.allow(url) {
        let _ = client
            .send(
                "Fetch.continueRequest",
                json!({ "requestId": request_id }),
                session,
            )
            .await;
    } else {
        blocked.fetch_add(1, Ordering::Relaxed);
        let _ = client
            .send(
                "Fetch.failRequest",
                json!({ "requestId": request_id, "errorReason": "BlockedByClient" }),
                session,
            )
            .await;
    }
}

/// Read a required string field from a CDP result, or a protocol error.
fn string_field(value: &Value, field: &str) -> Result<String, RenderError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| RenderError::Protocol(format!("missing `{field}` in CDP result")))
}
