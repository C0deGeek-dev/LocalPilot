//! The host seam for reaching the web on the user's behalf.
//!
//! Tools never build an HTTP client themselves for the open web. A host that
//! can ask a person for consent and keep an egress audit supplies a
//! [`WebGateway`]; the session hands it to tools through
//! [`crate::ToolContext::web`] together with the session's live privacy
//! state. A session with no gateway has no web access, and the
//! web tools say so instead of reaching out.

use std::time::Duration;

use async_trait::async_trait;
use localpilot_sandbox::Interactivity;

/// What a session lends a web tool for one call.
#[derive(Clone, Copy)]
pub struct WebContext<'a> {
    /// The host's gated web access. It asks for web consent on a channel of its
    /// own that only the person at this host answers — never through the
    /// permission profile or the session's shared approver, which other devices
    /// or automation may answer.
    pub gateway: &'a dyn WebGateway,
    /// Whether the session is incognito right now.
    pub incognito: bool,
}

/// One page retrieval a tool asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebFetchRequest {
    /// The http or https URL to retrieve.
    pub url: String,
    /// The caller's preferred body bound; the gateway applies its own ceiling.
    pub max_bytes: Option<usize>,
    /// The caller's preferred total timeout; the gateway applies its own ceiling.
    pub timeout: Option<Duration>,
    /// The tool asking, named in the consent question.
    pub tool: String,
    /// Whether a person can be asked right now.
    pub interactivity: Interactivity,
    /// Whether the workspace is trusted.
    pub trusted: bool,
}

/// A retrieved page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebPage {
    /// Where the body came from after any followed redirects, with secrets in
    /// the URL removed.
    pub final_url: String,
    /// The HTTP status code.
    pub status: u16,
    /// The response `Content-Type`, lower-cased; empty when absent.
    pub content_type: String,
    /// The body as text: HTML reduced to readable Markdown, other text as sent.
    pub body: String,
    /// Whether the body was cut at the size bound.
    pub truncated: bool,
}

/// Why a web request was not made or did not produce a page. Each carries the
/// explanation the model sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebRefusal {
    /// Web access is switched off for chat (or for the whole workspace).
    Disabled(String),
    /// The session is incognito, and web access needs an audit record.
    Incognito(String),
    /// Nobody can be asked for consent in this session.
    NobodyToAsk(String),
    /// The user said no.
    Declined(String),
    /// The destination is not allowed (scheme, internal address, deny list).
    Blocked(String),
    /// The request was allowed but did not complete.
    Failed(String),
}

impl WebRefusal {
    /// The explanation for the model.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::Disabled(message)
            | Self::Incognito(message)
            | Self::NobodyToAsk(message)
            | Self::Declined(message)
            | Self::Blocked(message)
            | Self::Failed(message) => message,
        }
    }
}

/// Gated web access a host provides to the session's tools.
#[async_trait]
pub trait WebGateway: Send + Sync {
    /// Retrieve one page, asking for consent and writing the audit record as
    /// the host's rules require.
    async fn fetch(
        &self,
        request: WebFetchRequest,
        context: WebContext<'_>,
    ) -> Result<WebPage, WebRefusal>;

    /// Forget every consent given in this session. Called when the session
    /// changes (a new session, entering or leaving incognito).
    fn reset_consent(&self);
}

/// A URL as it may be written to a log, shown in a question, or sent to
/// another device: no user name or password, no fragment, and every query value
/// replaced, so a token carried in a link never lands in a durable record.
/// Parameter names stay, so the record still says what kind of request it was;
/// a query part that is not a `name=value` pair is replaced whole.
#[must_use]
pub fn redact_url(url: &str) -> String {
    let Ok(mut parsed) = reqwest::Url::parse(url) else {
        return "(unparseable URL)".to_string();
    };
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    parsed.set_fragment(None);
    let query = parsed.query().map(|query| {
        query
            .split('&')
            .filter(|part| !part.is_empty())
            .map(|part| match part.split_once('=') {
                Some((name, _)) => format!("{name}=redacted"),
                None => "redacted".to_string(),
            })
            .collect::<Vec<_>>()
            .join("&")
    });
    parsed.set_query(query.as_deref().filter(|query| !query.is_empty()));
    parsed.to_string()
}
