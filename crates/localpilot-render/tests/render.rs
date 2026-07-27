//! End-to-end tests that drive a real headless system browser. They run only
//! when a Chromium-family browser is present (so CI without one skips them, and
//! a developer machine with Chrome/Edge exercises the full CDP path); the fixture
//! is always served on loopback, so no external egress occurs.

use std::time::Duration;

use localpilot_render::ChromiumRenderer;
use localpilot_research::{
    RenderBounds, RenderFailure, RenderGate, RenderRequest, RenderedDoc, Renderer,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A gate that allows only URLs under a single origin prefix — the fixture's
/// own server — and blocks everything else.
struct OriginGate {
    allowed_prefix: String,
}

impl RenderGate for OriginGate {
    fn allow(&self, url: &str) -> bool {
        url.starts_with(&self.allowed_prefix)
    }
}

fn bounds() -> RenderBounds {
    RenderBounds {
        settle: Duration::from_millis(400),
        timeout: Duration::from_secs(15),
        max_frames: 8,
        max_depth: 3,
    }
}

/// Unwrap a render result, or skip the test when this machine has a browser that
/// will not *start*.
///
/// `ChromiumRenderer::available()` answers "is a browser installed", which is not
/// the same question as "will it run here". A CI runner can ship Chrome and still
/// refuse to launch it — a restricted user namespace blocks the sandbox, and the
/// browser exits before its devtools endpoint exists. That is the environment
/// failing, not the renderer, and it is the case the file header already means by
/// "run only when a browser is present".
///
/// Only launch-phase failures skip. A browser that starts and then renders the
/// wrong thing, drops a frame, or ignores the egress gate still fails the test:
/// those are the assertions this file exists for.
fn rendered_or_skip(result: Result<RenderedDoc, RenderFailure>, what: &str) -> Option<RenderedDoc> {
    match result {
        Ok(doc) => Some(doc),
        Err(RenderFailure::Unavailable) => {
            eprintln!("no usable browser; skipping renderer e2e test ({what})");
            None
        }
        Err(RenderFailure::Browser(detail)) if is_launch_failure(&detail) => {
            eprintln!("browser present but would not start; skipping ({what}): {detail}");
            None
        }
        Err(other) => panic!("{what}: {other:?}"),
    }
}

/// Whether a browser-error detail describes a failure to get the browser running
/// at all, as opposed to a failure of the render itself.
fn is_launch_failure(detail: &str) -> bool {
    detail.contains("browser exited before its devtools endpoint came up")
        || detail.contains("devtools endpoint did not come up in time")
        || detail.contains("browser launch failed")
        || detail.contains("no chromium-family browser found")
}

#[tokio::test]
async fn renders_javascript_injected_content() {
    if !ChromiumRenderer::available() {
        eprintln!("no chromium-family browser found; skipping renderer e2e test");
        return;
    }
    let server = MockServer::start().await;
    // A shell whose real content is written by JavaScript — synchronously on
    // parse and again via a delayed timer (the settle window must catch both).
    let page = "<html><body><div id=\"root\"></div><script>\
                document.getElementById('root').textContent = 'SYNC_RENDER_MARKER';\
                setTimeout(function(){ \
                  var d = document.createElement('p'); d.textContent = 'ASYNC_RENDER_MARKER'; \
                  document.body.appendChild(d); }, 60);\
                </script></body></html>";
    Mock::given(method("GET"))
        .and(path("/app"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(page.as_bytes(), "text/html"))
        .mount(&server)
        .await;
    let url = format!("{}/app", server.uri());
    let gate = OriginGate {
        allowed_prefix: server.uri(),
    };
    let renderer = ChromiumRenderer::new();

    let result = renderer
        .render(
            &RenderRequest {
                url: url.clone(),
                bounds: bounds(),
            },
            &gate,
        )
        .await;
    let Some(doc) = rendered_or_skip(result, "render succeeds against the local fixture") else {
        return;
    };

    assert!(
        doc.html.contains("SYNC_RENDER_MARKER"),
        "synchronously-injected content is in the rendered DOM: {}",
        doc.html
    );
    assert!(
        doc.html.contains("ASYNC_RENDER_MARKER"),
        "delayed-timer content is captured within the settle window: {}",
        doc.html
    );
}

#[tokio::test]
async fn extracts_same_origin_and_srcdoc_frame_documents() {
    if !ChromiumRenderer::available() {
        eprintln!("no chromium-family browser found; skipping renderer e2e test");
        return;
    }
    let server = MockServer::start().await;
    let child =
        "<html><body><article>SAME_ORIGIN_FRAME_MARKER documentation body.</article></body></html>";
    Mock::given(method("GET"))
        .and(path("/frame-child"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(child.as_bytes(), "text/html"))
        .mount(&server)
        .await;
    let parent = format!(
        "<html><body><div id=\"root\">parent</div>\
         <iframe src=\"{}/frame-child\"></iframe>\
         <iframe srcdoc=\"<html><body><p>SRCDOC_FRAME_MARKER</p></body></html>\"></iframe>\
         </body></html>",
        server.uri()
    );
    Mock::given(method("GET"))
        .and(path("/parent"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(parent.as_bytes(), "text/html"))
        .mount(&server)
        .await;
    let url = format!("{}/parent", server.uri());
    let gate = OriginGate {
        allowed_prefix: server.uri(),
    };
    let renderer = ChromiumRenderer::new();

    let result = renderer
        .render(
            &RenderRequest {
                url,
                bounds: bounds(),
            },
            &gate,
        )
        .await;
    let Some(doc) = rendered_or_skip(result, "render succeeds") else {
        return;
    };

    assert!(
        doc.frames.len() >= 2,
        "both the same-origin and srcdoc frames are extracted: {} frames",
        doc.frames.len()
    );
    let all_frames: String = doc.frames.iter().map(|f| f.html.as_str()).collect();
    assert!(
        all_frames.contains("SAME_ORIGIN_FRAME_MARKER"),
        "same-origin frame document extracted: {all_frames}"
    );
    assert!(
        all_frames.contains("SRCDOC_FRAME_MARKER"),
        "srcdoc frame document extracted: {all_frames}"
    );
}

#[tokio::test]
async fn gate_blocks_a_disallowed_subresource_and_counts_it() {
    if !ChromiumRenderer::available() {
        eprintln!("no chromium-family browser found; skipping renderer e2e test");
        return;
    }
    let server = MockServer::start().await;
    // The page references a script on a different origin the gate disallows; the
    // page's own inline content still renders, and the blocked request is counted.
    let page = "<html><body><div id=\"root\"></div>\
                <script src=\"http://disallowed.invalid/evil.js\"></script>\
                <script>document.getElementById('root').textContent = 'PAGE_OK';</script>\
                </body></html>";
    Mock::given(method("GET"))
        .and(path("/app"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(page.as_bytes(), "text/html"))
        .mount(&server)
        .await;
    let url = format!("{}/app", server.uri());
    let gate = OriginGate {
        allowed_prefix: server.uri(),
    };
    let renderer = ChromiumRenderer::new();

    let result = renderer
        .render(
            &RenderRequest {
                url,
                bounds: bounds(),
            },
            &gate,
        )
        .await;
    let Some(doc) = rendered_or_skip(result, "render succeeds even with a blocked subresource")
    else {
        return;
    };

    assert!(
        doc.html.contains("PAGE_OK"),
        "the allowed page still renders: {}",
        doc.html
    );
    assert!(
        doc.blocked >= 1,
        "the disallowed cross-origin subresource was blocked and counted (blocked={})",
        doc.blocked
    );
}
