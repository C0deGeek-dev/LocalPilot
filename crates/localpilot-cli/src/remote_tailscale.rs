//! Guided Tailscale reach: read what this computer's Tailscale daemon serves
//! and decide, without changing anything, whether the phone origin
//! (`https://<machine>.<tailnet>.ts.net/` on 443, its root forwarded to the
//! loopback gateway) can be added while leaving every other Serve setting as
//! it is.
//!
//! The daemon's local HTTP API is the surface (a server the user runs on this
//! computer). It is not a stable interface upstream, so anything this module
//! changes later is limited to stable releases from [`OLDEST`] on whose
//! install passes the behaviour checks; every other version is read for
//! diagnostics only. Reading is split from deciding: the
//! transport returns raw replies and [`assess`] is a pure function of them.

use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::{header, Method, Request};
use hyper_util::rt::TokioIo;
use serde_json::{Map, Value};
use tokio::io::{AsyncRead, AsyncWrite};

/// The `Host` every request carries; the daemon refuses any other.
const API_HOST: &str = "local-tailscaled.sock";
/// Replies larger than this are cut and marked truncated.
const MAX_REPLY: usize = 1024 * 1024;
/// One request, connect to last body byte.
const EXCHANGE_WITHIN: Duration = Duration::from_secs(5);
/// The phone origin's port.
const HTTPS_PORT: &str = "443";

/// The oldest Tailscale release setup may change: the first one whose local
/// API source was inspected for the conditional Serve write. Newer stable
/// releases qualify too; what protects them is behaviour, checked on the
/// install itself before any change: the Serve config must hold only
/// settings [`assess`] understands, and a write with a deliberately stale
/// version tag must be refused.
const OLDEST: (u64, u64, u64) = (1, 102, 0);

/// A complete answer from the daemon. The status is read before the body, so
/// a body cut at [`MAX_REPLY`] still carries a definite status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reply {
    pub status: u16,
    pub etag: Option<String>,
    pub body: Vec<u8>,
    pub truncated: bool,
}

/// Why no complete answer arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ApiFailure {
    /// Nothing reached the daemon: the connection was never made.
    Unreachable(String),
    /// The exchange started and did not finish (timeout, reset, protocol
    /// error); whether the daemon acted on the request is unknown.
    Interrupted(String),
}

impl std::fmt::Display for ApiFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(why) => {
                write!(f, "could not reach Tailscale on this computer: {why}")
            }
            Self::Interrupted(why) => {
                write!(f, "the exchange with Tailscale did not finish: {why}")
            }
        }
    }
}

/// What a request carries besides its method and path.
#[derive(Debug, Clone, Default)]
struct Payload {
    /// A JSON body.
    body: Option<Vec<u8>>,
    /// The version tag a write is conditional on.
    if_match: Option<String>,
    /// The access token the endpoint asks for, when it asks for one.
    token: Option<Token>,
}

/// The Tailscale app's access token for its local API. A credential: it is
/// never printed, logged, journaled or passed to another process, and its
/// `Debug` form hides it.
#[derive(Clone, PartialEq, Eq)]
struct Token(String);

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(hidden)")
    }
}

/// Where this computer's daemon answers.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Endpoint {
    /// The daemon's own pipe or socket, admitted by who the caller is.
    Daemon,
    /// The Tailscale app for macOS (the standalone build): a loopback port,
    /// admitted by an access token only administrators can read.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    App { port: u16, token: Token },
}

/// Send one GET to the daemon's local API.
pub(crate) async fn get(path: &str) -> Result<Reply, ApiFailure> {
    send(Method::GET, path, Payload::default()).await
}

/// Locate the endpoint, connect and run one request under one budget. The
/// app's endpoint answers 401 when the token is missing or not accepted; it
/// refuses before acting, so that is reported as unreachable, never as a
/// reply.
async fn send(method: Method, path: &str, mut payload: Payload) -> Result<Reply, ApiFailure> {
    let endpoint = locate()?;
    if let Endpoint::App { token, .. } = &endpoint {
        payload.token = Some(token.clone());
    }
    let read = method == Method::GET;
    let reply = request(connect(&endpoint), method, path, payload, EXCHANGE_WITHIN).await?;
    app_admitted(&endpoint, &reply, read)?;
    Ok(reply)
}

/// The app's endpoint refuses a missing or wrong token with 401 or 403
/// before acting on the request. A 401 is always that; a 403 is that for a
/// read (a write's 403 is the caller's to report, as on every platform).
fn app_admitted(endpoint: &Endpoint, reply: &Reply, read: bool) -> Result<(), ApiFailure> {
    if !matches!(endpoint, Endpoint::App { .. }) {
        return Ok(());
    }
    if reply.status == 401 || (read && reply.status == 403) {
        return Err(ApiFailure::Unreachable(
            "the Tailscale app did not accept LocalPilot's access token; it may have just \
             restarted. Try again; if it keeps happening, quit and reopen the Tailscale app"
                .to_string(),
        ));
    }
    Ok(())
}

/// Send one conditional POST of a JSON document. An empty `if_match` is
/// refused here: the daemon would treat it as "overwrite whatever is there".
pub(crate) async fn post_json(
    path: &str,
    body: &Value,
    if_match: &str,
) -> Result<Reply, ApiFailure> {
    if if_match.trim().is_empty() {
        return Err(ApiFailure::Unreachable(
            "refusing an unconditional write".to_string(),
        ));
    }
    let payload = Payload {
        body: Some(json_bytes(body)?),
        if_match: Some(if_match.to_string()),
        token: None,
    };
    send(Method::POST, path, payload).await
}

/// Send one PATCH of a JSON document (the daemon has no conditional form).
pub(crate) async fn patch_json(path: &str, body: &Value) -> Result<Reply, ApiFailure> {
    let payload = Payload {
        body: Some(json_bytes(body)?),
        if_match: None,
        token: None,
    };
    send(Method::PATCH, path, payload).await
}

fn json_bytes(body: &Value) -> Result<Vec<u8>, ApiFailure> {
    serde_json::to_vec(body).map_err(|error| ApiFailure::Unreachable(error.to_string()))
}

/// One request under one budget, from before connecting to the last body
/// byte. Running out before the connection is made is
/// [`ApiFailure::Unreachable`]; after it, see [`exchange`].
async fn request<C, S>(
    connect: C,
    method: Method,
    path: &str,
    payload: Payload,
    within: Duration,
) -> Result<Reply, ApiFailure>
where
    C: std::future::Future<Output = Result<S, ApiFailure>>,
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let deadline = tokio::time::Instant::now() + within;
    let stream = match tokio::time::timeout_at(deadline, connect).await {
        Ok(stream) => stream?,
        Err(_) => return Err(ApiFailure::Unreachable("connecting timed out".to_string())),
    };
    exchange(stream, method, path, payload, deadline).await
}

/// A connected stream to whichever endpoint answers.
trait Stream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Stream for T {}

/// The open-source daemon's socket on macOS.
#[cfg_attr(windows, allow(dead_code))]
const MACOS_SOCKET: &str = "/var/run/tailscaled.socket";
/// Where the Tailscale app for macOS (the standalone build) records its port
/// and access token.
#[cfg_attr(windows, allow(dead_code))]
const APP_DIR: &str = "/Library/Tailscale";
/// An access token longer than this is not one the app wrote.
#[cfg_attr(windows, allow(dead_code))]
const TOKEN_MAX: u64 = 256;

#[cfg(windows)]
fn locate() -> Result<Endpoint, ApiFailure> {
    Ok(Endpoint::Daemon)
}

#[cfg(windows)]
async fn connect(_endpoint: &Endpoint) -> Result<Box<dyn Stream>, ApiFailure> {
    // The pipe sits under the administrators-only prefix, so whatever answers
    // there was created by the service (or an administrator), not another user.
    const PIPE: &str = r"\\.\pipe\ProtectedPrefix\Administrators\Tailscale\tailscaled";
    let pipe = tokio::net::windows::named_pipe::ClientOptions::new()
        .open(PIPE)
        .map_err(|error| ApiFailure::Unreachable(unreachable_reason(&error)))?;
    Ok(Box::new(pipe))
}

/// On macOS the open-source daemon's socket comes first; without it, the
/// Tailscale app's recorded port and token. Every other Unix system has the
/// daemon's socket only.
#[cfg(unix)]
fn locate() -> Result<Endpoint, ApiFailure> {
    if cfg!(target_os = "macos") && !std::path::Path::new(MACOS_SOCKET).exists() {
        return app_endpoint(std::path::Path::new(APP_DIR), 0);
    }
    Ok(Endpoint::Daemon)
}

#[cfg(unix)]
async fn connect(endpoint: &Endpoint) -> Result<Box<dyn Stream>, ApiFailure> {
    // Both are bounded by the caller's one budget.
    match endpoint {
        Endpoint::Daemon => {
            let path = socket_path()?;
            let stream = tokio::net::UnixStream::connect(&path)
                .await
                .map_err(|error| ApiFailure::Unreachable(unreachable_reason(&error)))?;
            Ok(Box::new(stream))
        }
        Endpoint::App { port, .. } => {
            let stream = tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, *port))
                .await
                .map_err(|error| ApiFailure::Unreachable(app_unreachable_reason(&error)))?;
            Ok(Box::new(stream))
        }
    }
}

#[cfg(unix)]
fn socket_path() -> Result<std::path::PathBuf, ApiFailure> {
    if cfg!(target_os = "macos") {
        return Ok(std::path::PathBuf::from(MACOS_SOCKET));
    }
    if std::path::Path::new("/var/run").is_dir() {
        Ok(std::path::PathBuf::from(
            "/var/run/tailscale/tailscaled.sock",
        ))
    } else {
        Err(ApiFailure::Unreachable(
            "this system has no /var/run, so the Tailscale socket location is unknown".to_string(),
        ))
    }
}

#[cfg(not(any(windows, unix)))]
fn locate() -> Result<Endpoint, ApiFailure> {
    Ok(Endpoint::Daemon)
}

#[cfg(not(any(windows, unix)))]
async fn connect(_endpoint: &Endpoint) -> Result<Box<dyn Stream>, ApiFailure> {
    Err(ApiFailure::Unreachable(
        "setup does not support this operating system".to_string(),
    ))
}

/// The Tailscale app's endpoint from the records in `dir`: a link whose
/// target is the port, and the access token for that port. Both must belong
/// to `owner` (root for the real app), so another local user cannot point
/// LocalPilot at a port of their own. The records can outlive the app; a
/// port nothing answers on is reported when connecting.
#[cfg(unix)]
fn app_endpoint(dir: &std::path::Path, owner: u32) -> Result<Endpoint, ApiFailure> {
    use std::os::unix::fs::MetadataExt as _;
    let link = dir.join("ipnport");
    let record = match std::fs::symlink_metadata(&link) {
        Ok(record) => record,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ApiFailure::Unreachable(
                "Tailscale is not running: this Mac has no Tailscale daemon socket and no \
                 Tailscale app port. The App Store build of the Tailscale app is not \
                 supported by setup yet; the standalone build from tailscale.com and the \
                 open-source tailscaled are"
                    .to_string(),
            ));
        }
        Err(error) => {
            return Err(ApiFailure::Unreachable(format!(
                "the Tailscale app's port record could not be read ({})",
                error.kind()
            )));
        }
    };
    if !record.file_type().is_symlink() || record.uid() != owner {
        return Err(ApiFailure::Unreachable(
            "the Tailscale app's port record is not the root-owned link the app writes".to_string(),
        ));
    }
    let port = std::fs::read_link(&link)
        .ok()
        .as_deref()
        .and_then(port_from_link)
        .ok_or_else(|| {
            ApiFailure::Unreachable(
                "the Tailscale app's port record does not name a port".to_string(),
            )
        })?;
    let token = read_token(&dir.join(format!("sameuserproof-{port}")), owner)?;
    Ok(Endpoint::App { port, token })
}

#[cfg(unix)]
fn read_token(path: &std::path::Path, owner: u32) -> Result<Token, ApiFailure> {
    use std::io::Read as _;
    use std::os::unix::fs::MetadataExt as _;
    let file = std::fs::File::open(path).map_err(|error| {
        ApiFailure::Unreachable(match error.kind() {
            std::io::ErrorKind::PermissionDenied => {
                "this user is not an administrator on this Mac, so it may not read the \
                 Tailscale app's access token"
                    .to_string()
            }
            std::io::ErrorKind::NotFound => {
                "the Tailscale app's access token for its recorded port is missing; the app \
                 may be restarting. Try again in a moment"
                    .to_string()
            }
            kind => format!("the Tailscale app's access token could not be read ({kind})"),
        })
    })?;
    let malformed = || {
        ApiFailure::Unreachable(
            "the Tailscale app's access token is not one the app writes".to_string(),
        )
    };
    let record = file.metadata().map_err(|_| malformed())?;
    if !record.is_file() || record.uid() != owner || record.len() > TOKEN_MAX {
        return Err(malformed());
    }
    let mut bytes = Vec::new();
    file.take(TOKEN_MAX + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| malformed())?;
    token_from_bytes(&bytes).ok_or_else(malformed)
}

/// The port a port record's link points at: decimal digits only, 1 to 65535.
#[cfg_attr(windows, allow(dead_code))]
fn port_from_link(target: &std::path::Path) -> Option<u16> {
    let text = target.to_str()?;
    if text.is_empty() || text.len() > 5 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse::<u16>().ok().filter(|port| *port != 0)
}

/// A token as the app writes it: printable ASCII without spaces, surrounding
/// line breaks allowed, bounded in length.
#[cfg_attr(windows, allow(dead_code))]
fn token_from_bytes(bytes: &[u8]) -> Option<Token> {
    let text = std::str::from_utf8(bytes).ok()?.trim_ascii();
    let fits = u64::try_from(text.len()).is_ok_and(|len| len <= TOKEN_MAX);
    (fits && !text.is_empty() && text.bytes().all(|b| b.is_ascii_graphic()))
        .then(|| Token(text.to_string()))
}

#[cfg_attr(windows, allow(dead_code))]
fn app_unreachable_reason(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::ConnectionRefused => {
            "the Tailscale app is not running (nothing answers on the port it recorded)".to_string()
        }
        _ => unreachable_reason(error),
    }
}

/// HTTP Basic credentials with an empty user name and the token as the
/// password, marked sensitive so it is kept out of any header dump.
fn basic_auth(token: &Token) -> Result<header::HeaderValue, ApiFailure> {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!(":{}", token.0));
    let mut value = header::HeaderValue::from_str(&format!("Basic {encoded}")).map_err(|_| {
        ApiFailure::Unreachable("the Tailscale app's access token could not be sent".to_string())
    })?;
    value.set_sensitive(true);
    Ok(value)
}

fn unreachable_reason(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound => {
            "Tailscale is not running (no daemon endpoint on this computer)".to_string()
        }
        std::io::ErrorKind::PermissionDenied => {
            "this user may not talk to Tailscale's daemon".to_string()
        }
        _ => error.to_string(),
    }
}

/// One HTTP/1.1 request over an already connected stream, ending by
/// `deadline` and bounded in size. Before the response head arrives, any
/// failure or the deadline is [`ApiFailure::Interrupted`]: whether the daemon
/// acted is unknown. Once the head has arrived its status and tag are
/// definite, so a body that breaks, stalls past the deadline or exceeds
/// [`MAX_REPLY`] gives a truncated [`Reply`], never an unknown outcome.
async fn exchange<S>(
    stream: S,
    method: Method,
    path: &str,
    payload: Payload,
    deadline: tokio::time::Instant,
) -> Result<Reply, ApiFailure>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, API_HOST);
    if payload.body.is_some() {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    if let Some(tag) = &payload.if_match {
        builder = builder.header(header::IF_MATCH, tag.as_str());
    }
    if let Some(token) = &payload.token {
        builder = builder.header(header::AUTHORIZATION, basic_auth(token)?);
    }
    let request = builder
        .body(Full::new(Bytes::from(payload.body.unwrap_or_default())))
        .map_err(|error| ApiFailure::Unreachable(error.to_string()))?;
    let handshake = hyper::client::conn::http1::handshake(TokioIo::new(stream));
    let (mut sender, connection) = match tokio::time::timeout_at(deadline, handshake).await {
        Ok(Ok(pair)) => pair,
        Ok(Err(error)) => return Err(ApiFailure::Interrupted(error.to_string())),
        Err(_) => return Err(ApiFailure::Interrupted("timed out".to_string())),
    };
    let driver = tokio::spawn(connection);
    let response = match tokio::time::timeout_at(deadline, sender.send_request(request)).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            driver.abort();
            return Err(ApiFailure::Interrupted(error.to_string()));
        }
        Err(_) => {
            driver.abort();
            return Err(ApiFailure::Interrupted("timed out".to_string()));
        }
    };
    let status = response.status().as_u16();
    let etag = response
        .headers()
        .get(header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let mut body = response.into_body();
    let mut collected = Vec::new();
    let mut truncated = false;
    loop {
        let frame = match tokio::time::timeout_at(deadline, body.frame()).await {
            Ok(Some(Ok(frame))) => frame,
            Ok(None) => break,
            // A body that breaks or stalls after the head: a cut reply.
            Ok(Some(Err(_))) | Err(_) => {
                truncated = true;
                break;
            }
        };
        if let Ok(data) = frame.into_data() {
            let room = MAX_REPLY - collected.len();
            if data.len() > room {
                collected.extend_from_slice(&data[..room]);
                truncated = true;
                break;
            }
            collected.extend_from_slice(&data);
        }
    }
    driver.abort();
    Ok(Reply {
        status,
        etag,
        body: collected,
        truncated,
    })
}

/// Whether setup may change Serve on this daemon version. A stable release
/// build reports `<major>.<minor>.<patch>-t<commit>`, optionally followed by
/// `-g<commit>` (the commit of the separate repository its packaging is built
/// from), with an even minor; an odd minor is Tailscale's unstable track. A
/// build with a change count after the version (`1.104.5-3-t…`), a
/// development stamp or any other shape does not qualify, nor does anything
/// older than [`OLDEST`].
pub(crate) fn supported(version: &str) -> bool {
    let mut parts = version.split('-');
    let (Some(number), Some(commit)) = (parts.next(), parts.next()) else {
        return false;
    };
    if !commit.strip_prefix('t').is_some_and(short_hash) {
        return false;
    }
    if let Some(extra) = parts.next() {
        if !extra.strip_prefix('g').is_some_and(short_hash) || parts.next().is_some() {
            return false;
        }
    }
    let mut numbers = number.split('.').map(|part| {
        (!part.is_empty() && part.len() <= 6 && part.bytes().all(|b| b.is_ascii_digit()))
            .then(|| part.parse::<u64>().ok())
            .flatten()
    });
    let (Some(Some(major)), Some(Some(minor)), Some(Some(patch)), None) = (
        numbers.next(),
        numbers.next(),
        numbers.next(),
        numbers.next(),
    ) else {
        return false;
    };
    minor % 2 == 0 && (major, minor, patch) >= OLDEST
}

fn short_hash(text: &str) -> bool {
    text.len() == 9 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// What the daemon's status says about this node, read strictly: a field that
/// decides exposure and is missing or of the wrong type refuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Node {
    pub version: String,
    pub state: String,
    /// The node's MagicDNS name without the trailing dot.
    pub name: String,
    pub node_id: String,
    pub tailnet: String,
    pub suffix: String,
    pub magic_dns: bool,
    pub https: bool,
    pub cert_domains: Vec<String>,
}

fn text<'a>(object: &'a Map<String, Value>, key: &str, at: &str) -> Result<&'a str, Refusal> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Refusal::Malformed(format!("{at}{key} is missing or not text")))
}

fn object<'a>(value: &'a Value, at: &str) -> Result<&'a Map<String, Value>, Refusal> {
    value
        .as_object()
        .ok_or_else(|| Refusal::Malformed(format!("{at} is not an object")))
}

/// A non-empty identity field: a plan for "no node" is no plan.
fn identity(object: &Map<String, Value>, key: &str, at: &str) -> Result<String, Refusal> {
    let value = text(object, key, at)?;
    if value.trim().is_empty() {
        return Err(Refusal::Malformed(format!("{at}{key} is empty")));
    }
    Ok(value.to_string())
}

pub(crate) fn read_node(status: &Value) -> Result<Node, Refusal> {
    let top = object(status, "the status")?;
    let own = object(top.get("Self").unwrap_or(&Value::Null), "Self")?;
    let tailnet = object(
        top.get("CurrentTailnet").unwrap_or(&Value::Null),
        "CurrentTailnet",
    )?;
    let magic_dns = tailnet
        .get("MagicDNSEnabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            Refusal::Malformed("CurrentTailnet.MagicDNSEnabled is missing or not true/false".into())
        })?;
    // No capability map means no capabilities: fail closed, not malformed.
    let https = match own.get("CapMap") {
        None | Some(Value::Null) => false,
        Some(Value::Object(caps)) => caps.contains_key("https"),
        Some(_) => return Err(Refusal::Malformed("Self.CapMap is not an object".into())),
    };
    let cert_domains = match top.get("CertDomains") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| Refusal::Malformed("CertDomains holds a non-text entry".into()))
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(Refusal::Malformed("CertDomains is not a list".into())),
    };
    Ok(Node {
        version: text(top, "Version", "")?.to_string(),
        state: text(top, "BackendState", "")?.to_string(),
        name: text(own, "DNSName", "Self.")?
            .trim_end_matches('.')
            .to_string(),
        node_id: identity(own, "ID", "Self.")?,
        tailnet: identity(tailnet, "Name", "CurrentTailnet.")?,
        suffix: text(tailnet, "MagicDNSSuffix", "CurrentTailnet.")?
            .trim_end_matches('.')
            .to_string(),
        magic_dns,
        https,
        cert_domains,
    })
}

/// Why setup will not add the phone origin. Every variant is decided before
/// anything changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// A field that decides exposure is missing or has the wrong shape.
    Malformed(String),
    Unsupported(String),
    NotRunning(String),
    NoMagicDns,
    /// The node's name is not exactly one label inside the tailnet's suffix.
    NameOutsideTailnet {
        name: String,
        suffix: String,
    },
    NoHttps,
    NoCertificate(String),
    /// The Serve config came without a version tag, so a write could not be
    /// made conditional.
    NoTag,
    /// Something else already uses the phone origin.
    InUse(String),
}

impl Refusal {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Malformed(what) => format!("Tailscale's answer was not understood: {what}"),
            Self::Unsupported(version) => {
                format!(
                    "Tailscale {version} is not a stable release setup can change \
                     (1.102.0 or later)"
                )
            }
            Self::NotRunning(state) => format!("Tailscale is not connected (state: {state})"),
            Self::NoMagicDns => "MagicDNS is off for this tailnet".to_string(),
            Self::NameOutsideTailnet { name, suffix } => {
                format!("this computer's name {name:?} is not a machine name inside {suffix:?}")
            }
            Self::NoHttps => "HTTPS certificates are not enabled for this tailnet".to_string(),
            Self::NoCertificate(name) => {
                format!("Tailscale offers no certificate for {name}")
            }
            Self::NoTag => {
                "Tailscale did not say which version of its Serve settings it returned".to_string()
            }
            Self::InUse(what) => format!("the phone address is already in use: {what}"),
        }
    }

    pub(crate) fn remedy(&self) -> &'static str {
        match self {
            Self::Malformed(_) | Self::NoTag => {
                "Nothing was changed. Update Tailscale or LocalPilot, or set the phone address \
                 up yourself (docs: configuration, remote access)."
            }
            Self::Unsupported(_) => {
                "Nothing was changed. Install a current stable Tailscale release, or set the phone \
                 address up yourself with `tailscale serve` (docs: configuration, remote access)."
            }
            Self::NotRunning(_) => "Sign in to Tailscale on this computer, then run this again.",
            Self::NoMagicDns | Self::NoHttps | Self::NoCertificate(_) => {
                "In the Tailscale admin console, under DNS, turn on MagicDNS and HTTPS \
                 Certificates, then run this again."
            }
            Self::NameOutsideTailnet { .. } => {
                "Nothing was changed. Check this computer's name in the Tailscale admin console."
            }
            Self::InUse(_) => {
                "Nothing was changed. Remove that Serve setting yourself if you no longer need \
                 it (`tailscale serve status` shows it), then run this again."
            }
        }
    }
}

/// The one change setup would make.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Change {
    /// `https://<name>`.
    pub origin: String,
    /// The node and tailnet the change was planned for.
    #[cfg_attr(not(test), allow(dead_code))]
    pub node_id: String,
    #[cfg_attr(not(test), allow(dead_code))]
    pub tailnet: String,
    /// The full Serve config to write: what was read, plus the phone origin.
    #[cfg_attr(not(test), allow(dead_code))]
    pub config: Value,
    /// The version tag the write must be conditional on.
    pub if_match: String,
    /// Set when the existing config serves a file, directory or Unix socket:
    /// Tailscale then lets only an administrator write any change.
    pub admin_note: Option<String>,
}

/// What setup would do on this computer.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Readiness {
    Ready(Change),
    /// Exactly the phone origin is already served, by someone else's setup:
    /// it is used as it is and never removed.
    AlreadyServed {
        origin: String,
    },
    Refused(Refusal),
}

/// Decide, from one status and one Serve config read with its version tag,
/// what setup would do. Pure: the same inputs give the same answer.
pub(crate) fn assess(status: &Value, config: &Value, etag: Option<&str>, port: u16) -> Readiness {
    match assess_inner(status, config, etag, port) {
        Ok(readiness) => readiness,
        Err(refusal) => Readiness::Refused(refusal),
    }
}

fn assess_inner(
    status: &Value,
    config: &Value,
    etag: Option<&str>,
    port: u16,
) -> Result<Readiness, Refusal> {
    let node = eligible(status)?;
    let if_match = version_tag(etag)?;
    let origin = format!("https://{}", node.name);
    match origin_plan(config, &node.name, port)? {
        OriginPlan::AlreadyServed => Ok(Readiness::AlreadyServed { origin }),
        OriginPlan::Free(planned) => Ok(Readiness::Ready(Change {
            origin,
            node_id: node.node_id,
            tailnet: node.tailnet,
            config: planned,
            if_match,
            admin_note: admin_note(config),
        })),
    }
}

/// The observed node's prerequisites: a stable release setup may change,
/// connected, MagicDNS on, a one-label name inside the tailnet, HTTPS
/// certificates on and a certificate offered for the node's own current name.
pub(crate) fn eligible(status: &Value) -> Result<Node, Refusal> {
    let node = read_node(status)?;
    if !supported(&node.version) {
        return Err(Refusal::Unsupported(node.version));
    }
    if node.state != "Running" {
        return Err(Refusal::NotRunning(node.state));
    }
    if !node.magic_dns {
        return Err(Refusal::NoMagicDns);
    }
    if !one_label(&node.name, &node.suffix) {
        return Err(Refusal::NameOutsideTailnet {
            name: node.name,
            suffix: node.suffix,
        });
    }
    if !node.https {
        return Err(Refusal::NoHttps);
    }
    if !node.cert_domains.iter().any(|domain| domain == &node.name) {
        return Err(Refusal::NoCertificate(node.name));
    }
    Ok(node)
}

/// Whether `name` is exactly one label followed by `.` and `suffix`.
pub(crate) fn one_label(name: &str, suffix: &str) -> bool {
    let label = name
        .strip_suffix(suffix)
        .and_then(|rest| rest.strip_suffix('.'));
    !suffix.is_empty() && matches!(label, Some(label) if !label.is_empty() && !label.contains('.'))
}

/// The version tag a write must be conditional on; none means no write.
pub(crate) fn version_tag(etag: Option<&str>) -> Result<String, Refusal> {
    match etag {
        Some(tag) if !tag.trim().is_empty() => Ok(tag.to_string()),
        _ => Err(Refusal::NoTag),
    }
}

/// The note shown when the existing config shares a file, folder or Unix
/// socket, so Tailscale accepts any change only from an administrator.
pub(crate) fn admin_note(config: &Value) -> Option<String> {
    serves_files_or_sockets(config).then(|| {
        "your existing Serve settings share a file, folder or Unix socket, so Tailscale \
         will accept a change only from an administrator"
            .to_string()
    })
}

/// What the phone origin on `name` would take.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum OriginPlan {
    /// Nothing is on the origin: the config to write is the one read plus the
    /// origin's two entries.
    Free(Value),
    /// Exactly the phone origin's two entries are already there.
    AlreadyServed,
}

/// Structure, schema and exclusive-origin checks for `https://<name>/` on
/// 443 forwarding to `127.0.0.1:<port>`, independent of what the node's
/// status says about certificates.
pub(crate) fn origin_plan(config: &Value, name: &str, port: u16) -> Result<OriginPlan, Refusal> {
    // The whole config is judged before any conflict is: an unknown setting
    // anywhere refuses, whatever else is found first.
    schema_ok(config)?;
    let host_port = format!("{name}:{HTTPS_PORT}");
    let (ours_tcp, ours_web) = our_entries(port);
    let mut top = match config {
        Value::Null => Map::new(),
        Value::Object(map) => map.clone(),
        _ => {
            return Err(Refusal::Malformed(
                "the Serve config is not an object".into(),
            ))
        }
    };
    let found = inspect(&top, &host_port, "")?;
    for (key, value) in &top {
        match key.as_str() {
            "Foreground" => {
                for (session, inner) in entries(value, "Foreground")? {
                    let inner = object(inner, &format!("Foreground.{session}"))?;
                    if inner.contains_key("Foreground") || inner.contains_key("Services") {
                        return Err(Refusal::Malformed(format!(
                            "Foreground.{session} nests a section it should not"
                        )));
                    }
                    let nested = inspect(inner, &host_port, &format!("Foreground.{session}."))?;
                    if nested.tcp.is_some() || nested.web.is_some() || nested.funnel {
                        return Err(Refusal::InUse(
                            "a `tailscale serve` session running in a terminal uses port 443"
                                .to_string(),
                        ));
                    }
                }
            }
            "Services" => {
                for (service, inner) in entries(value, "Services")? {
                    let inner = object(inner, &format!("Services.{service}"))?;
                    for (field, part) in inner {
                        match field.as_str() {
                            "TCP" => check_tcp(part, &format!("Services.{service}.TCP"))?,
                            "Web" => {
                                for (key, web) in entries(part, &format!("Services.{service}.Web"))?
                                {
                                    let at = format!("Services.{service}.Web");
                                    if origin_key(key, &host_port, &at)? {
                                        return Err(Refusal::InUse(format!(
                                            "the Tailscale service {service} serves this address"
                                        )));
                                    }
                                    check_web(web, &format!("Services.{service}.Web.{key}"))?;
                                }
                            }
                            "Tun" => {
                                if !part.is_boolean() {
                                    return Err(Refusal::Malformed(format!(
                                        "Services.{service}.Tun is not true/false"
                                    )));
                                }
                            }
                            other => {
                                return Err(Refusal::Malformed(format!(
                                    "Services.{service} has an unrecognised field {other}"
                                )))
                            }
                        }
                    }
                }
            }
            // Checked by `inspect`, which also refuses unrecognised sections.
            _ => {}
        }
    }
    if found.funnel {
        return Err(Refusal::InUse(
            "Funnel (public internet access) is configured for this address".to_string(),
        ));
    }
    match (found.tcp, found.web) {
        (None, None) => {}
        (Some(tcp), Some(web)) if tcp == &ours_tcp && web == &ours_web => {
            return Ok(OriginPlan::AlreadyServed);
        }
        (Some(_), _) => {
            return Err(Refusal::InUse(
                "port 443 already has a Serve setting".to_string(),
            ))
        }
        (None, Some(_)) => {
            return Err(Refusal::InUse(format!(
                "{host_port} already has web handlers"
            )))
        }
    }
    insert(&mut top, "TCP", HTTPS_PORT.to_string(), ours_tcp);
    insert(&mut top, "Web", host_port, ours_web);
    Ok(OriginPlan::Free(Value::Object(top)))
}

/// Whether every setting in `config` is one setup understands. Only the
/// shape is judged: an empty name matches no origin, and any "in use" answer
/// for it is not a schema failure.
pub(crate) fn schema_ok(config: &Value) -> Result<(), Refusal> {
    let top = match config {
        Value::Null => return Ok(()),
        Value::Object(map) => map,
        _ => {
            return Err(Refusal::Malformed(
                "the Serve config is not an object".into(),
            ))
        }
    };
    // `inspect` with a name no valid key can match judges shape only.
    inspect(top, "", "")?;
    for (key, value) in top {
        match key.as_str() {
            "Foreground" => {
                for (session, inner) in entries(value, "Foreground")? {
                    let inner = object(inner, &format!("Foreground.{session}"))?;
                    if inner.contains_key("Foreground") || inner.contains_key("Services") {
                        return Err(Refusal::Malformed(format!(
                            "Foreground.{session} nests a section it should not"
                        )));
                    }
                    inspect(inner, "", &format!("Foreground.{session}."))?;
                }
            }
            "Services" => {
                for (service, inner) in entries(value, "Services")? {
                    let inner = object(inner, &format!("Services.{service}"))?;
                    for (field, part) in inner {
                        match field.as_str() {
                            "TCP" => check_tcp(part, &format!("Services.{service}.TCP"))?,
                            "Web" => {
                                let at = format!("Services.{service}.Web");
                                for (key, web) in entries(part, &at)? {
                                    origin_key(key, "", &at)?;
                                    check_web(web, &format!("{at}.{key}"))?;
                                }
                            }
                            "Tun" if part.is_boolean() => {}
                            other => {
                                return Err(Refusal::Malformed(format!(
                                    "Services.{service}.{other} is not understood"
                                )))
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// The phone origin's two entries: port 443 as HTTPS, and the root of the
/// origin forwarded to the gateway.
fn our_entries(port: u16) -> (Value, Value) {
    (
        serde_json::json!({ "HTTPS": true }),
        serde_json::json!({
            "Handlers": { "/": { "Proxy": format!("http://127.0.0.1:{port}") } }
        }),
    )
}

/// Anything in a config (already accepted by [`origin_plan`]) that a new
/// name for this computer would move: a listener on any port of the node, a
/// web or Funnel entry for `name` at any port, at the top level or in a
/// terminal-run session, or a service serving `name`. Services under their
/// own names are not moved by renaming the computer.
pub(crate) fn name_in_use(config: &Value, name: &str) -> Option<String> {
    fn level(map: &Map<String, Value>, name: &str, at: &str) -> Option<String> {
        if let Some(Value::Object(ports)) = map.get("TCP") {
            if let Some(port) = ports.keys().next() {
                return Some(format!("{at}port {port} of this computer is served"));
            }
        }
        for section in ["Web", "AllowFunnel"] {
            if let Some(Value::Object(keys)) = map.get(section) {
                for key in keys.keys() {
                    let host = key.rsplit_once(':').map_or(key.as_str(), |(host, _)| host);
                    if host.eq_ignore_ascii_case(name) {
                        return Some(format!("{at}{key} has a {section} setting"));
                    }
                }
            }
        }
        None
    }
    let top = config.as_object()?;
    if let Some(found) = level(top, name, "") {
        return Some(found);
    }
    if let Some(Value::Object(sessions)) = top.get("Foreground") {
        for inner in sessions.values() {
            if let Some(found) = inner
                .as_object()
                .and_then(|inner| level(inner, name, "a terminal-run `tailscale serve`: "))
            {
                return Some(found);
            }
        }
    }
    if let Some(Value::Object(services)) = top.get("Services") {
        for (service, inner) in services {
            if let Some(Value::Object(keys)) = inner.get("Web") {
                for key in keys.keys() {
                    let host = key.rsplit_once(':').map_or(key.as_str(), |(host, _)| host);
                    if host.eq_ignore_ascii_case(name) {
                        return Some(format!("the Tailscale service {service} serves {key}"));
                    }
                }
            }
        }
    }
    None
}

/// The config without the phone origin's two entries, for undo. The caller
/// has seen [`origin_plan`] answer [`OriginPlan::AlreadyServed`] for the same
/// config, so both entries are exactly ours.
pub(crate) fn without_ours(config: &Value, name: &str) -> Value {
    let host_port = format!("{name}:{HTTPS_PORT}");
    let mut top = config.as_object().cloned().unwrap_or_default();
    for (section, key) in [("TCP", HTTPS_PORT.to_string()), ("Web", host_port)] {
        let now_empty = match top.get_mut(section) {
            Some(Value::Object(map)) => {
                let found = map
                    .keys()
                    .find(|existing| existing.eq_ignore_ascii_case(&key))
                    .cloned();
                if let Some(found) = found {
                    map.remove(&found);
                }
                map.is_empty()
            }
            _ => false,
        };
        if now_empty {
            top.remove(section);
        }
    }
    Value::Object(top)
}

/// What one config level holds for the phone origin.
struct Found<'a> {
    tcp: Option<&'a Value>,
    web: Option<&'a Value>,
    funnel: bool,
}

/// Check the shapes of one level's TCP, Web and AllowFunnel, and pick out
/// anything on the phone origin.
fn inspect<'a>(
    level: &'a Map<String, Value>,
    host_port: &str,
    at: &str,
) -> Result<Found<'a>, Refusal> {
    let mut found = Found {
        tcp: None,
        web: None,
        funnel: false,
    };
    if let Some(tcp) = level.get("TCP") {
        check_tcp(tcp, &format!("{at}TCP"))?;
        found.tcp = tcp.as_object().and_then(|ports| ports.get(HTTPS_PORT));
    }
    if let Some(web) = level.get("Web") {
        for (key, site) in entries(web, &format!("{at}Web"))? {
            check_web(site, &format!("{at}Web.{key}"))?;
            if origin_key(key, host_port, &format!("{at}Web"))? {
                if found.web.is_some() {
                    return Err(Refusal::InUse(format!(
                        "{host_port} appears more than once in web handlers"
                    )));
                }
                found.web = Some(site);
            }
        }
    }
    if let Some(funnel) = level.get("AllowFunnel") {
        for (key, allowed) in entries(funnel, &format!("{at}AllowFunnel"))? {
            if !allowed.is_boolean() {
                return Err(Refusal::Malformed(format!(
                    "{at}AllowFunnel.{key} is not true/false"
                )));
            }
            // Any entry for the origin, even `false`, is a setting someone made.
            if origin_key(key, host_port, &format!("{at}AllowFunnel"))? {
                found.funnel = true;
            }
        }
    }
    for key in level.keys() {
        if !matches!(
            key.as_str(),
            "TCP" | "Web" | "AllowFunnel" | "Foreground" | "Services"
        ) {
            return Err(Refusal::Malformed(format!(
                "{at}{key} is an unrecognised section"
            )));
        }
    }
    Ok(found)
}

/// A port written the one way Tailscale writes it: decimal, no sign or
/// leading zero, in range.
fn canonical_port(text: &str) -> bool {
    text.parse::<u16>()
        .is_ok_and(|port| port != 0 && port.to_string() == text)
}

/// Check a Web or Funnel key (`<host>:<port>`, canonical port) and say
/// whether it names the phone origin. Host names compare without regard to
/// case, so a differently cased key is never a second, separate entry.
fn origin_key(key: &str, host_port: &str, at: &str) -> Result<bool, Refusal> {
    let fits = key
        .rsplit_once(':')
        .is_some_and(|(host, port)| !host.is_empty() && canonical_port(port));
    if !fits {
        return Err(Refusal::Malformed(format!(
            "{at} has a key {key:?} that is not host:port"
        )));
    }
    Ok(key.eq_ignore_ascii_case(host_port))
}

fn entries<'a>(value: &'a Value, at: &str) -> Result<&'a Map<String, Value>, Refusal> {
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(Refusal::Malformed(format!("{at} is not an object"))),
    }
}

fn check_tcp(value: &Value, at: &str) -> Result<(), Refusal> {
    for (port, handler) in entries(value, at)? {
        if !canonical_port(port) {
            return Err(Refusal::Malformed(format!(
                "{at} has a non-port key {port}"
            )));
        }
        for (field, part) in object(handler, &format!("{at}.{port}"))? {
            let fits = match field.as_str() {
                "HTTPS" | "HTTP" => part.is_boolean(),
                "TCPForward" | "TerminateTLS" => part.is_string(),
                // PROXY protocol version 1 or 2; 0 means none.
                "ProxyProtocol" => part.as_u64().is_some_and(|version| version <= 2),
                _ => false,
            };
            if !fits {
                return Err(Refusal::Malformed(format!(
                    "{at}.{port}.{field} is not understood"
                )));
            }
        }
    }
    Ok(())
}

fn check_web(value: &Value, at: &str) -> Result<(), Refusal> {
    let site = object(value, at)?;
    for (field, part) in site {
        if field != "Handlers" {
            return Err(Refusal::Malformed(format!(
                "{at}.{field} is not understood"
            )));
        }
        for (mount, handler) in entries(part, &format!("{at}.Handlers"))? {
            for (key, item) in object(handler, &format!("{at}.Handlers.{mount}"))? {
                let fits = match key.as_str() {
                    "Path" | "Proxy" | "Text" | "Redirect" => item.is_string(),
                    "AcceptAppCaps" => item
                        .as_array()
                        .is_some_and(|caps| caps.iter().all(Value::is_string)),
                    _ => false,
                };
                if !fits {
                    return Err(Refusal::Malformed(format!(
                        "{at}.Handlers.{mount}.{key} is not understood"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Whether any handler anywhere in the config shares a file, folder or Unix
/// socket. Tailscale checks the whole submitted config for these and then
/// requires an administrator, even when the new part is a plain proxy.
fn serves_files_or_sockets(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(key, item)| {
            let local = match (key.as_str(), item.as_str()) {
                ("Path", Some(path)) => !path.is_empty(),
                ("Proxy" | "TCPForward", Some(target)) => target.starts_with("unix:"),
                _ => false,
            };
            local || serves_files_or_sockets(item)
        }),
        Value::Array(items) => items.iter().any(serves_files_or_sockets),
        _ => false,
    }
}

/// Add `name: value` to the `key` section of the top level, creating the
/// section when it is absent (or null; `inspect` refused any other shape).
fn insert(top: &mut Map<String, Value>, key: &str, name: String, value: Value) {
    if let Some(Value::Object(section)) = top.get_mut(key) {
        section.insert(name, value);
        return;
    }
    let mut section = Map::new();
    section.insert(name, value);
    top.insert(key.to_string(), Value::Object(section));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    // Fixtures are shaped from the published Tailscale status and Serve config
    // types of the supported releases. Their shape (the fields read here and
    // their types, and a Serve config holding the phone origin) matches what
    // the Tailscale app for macOS 1.104.6 returned on a real install; the
    // values are placeholders, never captured ones.
    const NAME: &str = "pc.tail1234.ts.net";
    const HP: &str = "pc.tail1234.ts.net:443";
    const PORT: u16 = 47610;

    fn status() -> Value {
        json!({
            "Version": "1.104.5-t73a48057a-g15581c318",
            "BackendState": "Running",
            "Self": {
                "ID": "nABC123CNTRL",
                "DNSName": "pc.tail1234.ts.net.",
                "CapMap": { "https": null, "funnel": null }
            },
            "CurrentTailnet": {
                "Name": "user@example.com",
                "MagicDNSSuffix": "tail1234.ts.net",
                "MagicDNSEnabled": true
            },
            "CertDomains": ["pc.tail1234.ts.net"]
        })
    }

    fn with(mut base: Value, path: &[&str], value: Value) -> Value {
        let mut slot = &mut base;
        for key in &path[..path.len() - 1] {
            slot = &mut slot[*key];
        }
        slot[path[path.len() - 1]] = value;
        base
    }

    fn ours() -> (Value, Value) {
        (
            json!({ "HTTPS": true }),
            json!({ "Handlers": { "/": { "Proxy": "http://127.0.0.1:47610" } } }),
        )
    }

    fn refused(config: &Value) -> Refusal {
        match assess(&status(), config, Some("tag"), PORT) {
            Readiness::Refused(refusal) => refusal,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    fn refused_status(status: &Value) -> Refusal {
        match assess(status, &json!({}), Some("tag"), PORT) {
            Readiness::Refused(refusal) => refusal,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    fn ready(config: &Value) -> Change {
        match assess(&status(), config, Some("tag-1"), PORT) {
            Readiness::Ready(change) => change,
            other => panic!("expected ready, got {other:?}"),
        }
    }

    #[test]
    fn stable_releases_from_the_oldest_inspected_one_are_supported() {
        for yes in [
            "1.102.0-t35283c954",
            "1.104.5-t73a48057a",
            "1.104.5-t73a48057a-g15581c318",
            "1.106.2-t0123456ab",
            "1.200.0-t0123456ab",
            "2.0.0-t0123456ab",
        ] {
            assert!(supported(yes), "{yes}");
        }
        for no in [
            // Older than the oldest inspected release.
            "1.100.4-t0123456ab",
            "1.50.0-t0123456ab",
            // The unstable track.
            "1.105.0-t0123456ab",
            // A build past a release, a bare number, dev or odd shapes.
            "1.104.5-3-t73a48057a",
            "1.104.5",
            "1.104.5-dev20261103-t73a48057a",
            "1.104.5-t73a48057a-dirty",
            "1.104.5-t73a48057a-g15581c318-x",
            "1.104.5-t73a48057a-gXYZ",
            "1.104.5-t73A48057A",
            "1.104.5-t73a48057",
            "1.104.5-t73a48057a0",
            "v1.104.5-t73a48057a",
            "1.104-t73a48057a",
            "1.104.5.1-t73a48057a",
            "1.+104.5-t73a48057a",
            "1.104.5-ERR-BuildInfo",
            "",
        ] {
            assert!(!supported(no), "{no}");
        }
    }

    #[test]
    fn an_empty_config_gets_exactly_the_phone_origin() {
        let (tcp, web) = ours();
        for empty in [Value::Null, json!({})] {
            let change = ready(&empty);
            assert_eq!(change.origin, format!("https://{NAME}"));
            assert_eq!(change.if_match, "tag-1");
            assert_eq!(change.node_id, "nABC123CNTRL");
            assert_eq!(change.tailnet, "user@example.com");
            assert_eq!(change.admin_note, None);
            assert_eq!(
                change.config,
                json!({ "TCP": { "443": tcp.clone() }, "Web": { HP: web.clone() } })
            );
        }
    }

    #[test]
    fn everything_else_in_the_config_is_kept_as_it_was() {
        let before = json!({
            "TCP": { "8443": { "HTTPS": true }, "22": { "TCPForward": "127.0.0.1:22" } },
            "Web": { "pc.tail1234.ts.net:8443": { "Handlers": {
                "/": { "Proxy": "http://127.0.0.1:3000", "AcceptAppCaps": ["example.com/cap/x"] }
            } } },
            "AllowFunnel": { "pc.tail1234.ts.net:8443": true },
            "Foreground": { "session-1": { "TCP": { "10000": { "HTTPS": true } } } },
            "Services": { "svc:web": { "TCP": { "443": { "HTTPS": true } }, "Web": {
                "web.tail1234.ts.net:443": { "Handlers": { "/": { "Text": "hi" } } }
            }, "Tun": false } }
        });
        let change = ready(&before);
        let (tcp, web) = ours();
        let mut expected = before.clone();
        expected["TCP"]["443"] = tcp;
        expected["Web"][HP] = web;
        assert_eq!(change.config, expected);
    }

    #[test]
    fn the_identical_entry_is_reused_and_never_owned() {
        let (tcp, web) = ours();
        let config = json!({ "TCP": { "443": tcp }, "Web": { HP: web } });
        assert_eq!(
            assess(&status(), &config, Some("tag"), PORT),
            Readiness::AlreadyServed {
                origin: format!("https://{NAME}")
            }
        );
        // The same entries for a different gateway port are someone else's.
        assert!(matches!(
            assess(&status(), &config, Some("tag"), PORT + 1),
            Readiness::Refused(Refusal::InUse(_))
        ));
    }

    #[test]
    fn anything_already_on_the_origin_refuses() {
        let (tcp, web) = ours();
        let cases = [
            json!({ "TCP": { "443": tcp.clone() } }),
            json!({ "Web": { HP: web.clone() } }),
            json!({ "TCP": { "443": { "TCPForward": "127.0.0.1:8080" } } }),
            json!({ "TCP": { "443": tcp.clone() }, "Web": { HP: { "Handlers": {
                "/": { "Proxy": "http://127.0.0.1:47610" },
                "/other": { "Proxy": "http://127.0.0.1:9000" }
            } } } }),
            json!({ "TCP": { "443": tcp.clone() }, "Web": { HP: { "Handlers": {
                "/": { "Proxy": "http://127.0.0.1:3000" }
            } } } }),
            json!({ "AllowFunnel": { HP: false } }),
            json!({ "TCP": { "443": tcp.clone() }, "Web": { HP: web.clone() },
                    "AllowFunnel": { HP: true } }),
            json!({ "Foreground": { "s": { "TCP": { "443": { "HTTPS": true } } } } }),
            json!({ "Foreground": { "s": { "Web": { HP: web.clone() } } } }),
            json!({ "Foreground": { "s": { "AllowFunnel": { HP: true } } } }),
            json!({ "Services": { "svc:x": { "Web": { HP: web.clone() } } } }),
            // The same origin in other letter case is the same origin.
            json!({ "Web": { "PC.Tail1234.ts.net:443": web.clone() } }),
            json!({ "AllowFunnel": { "PC.TAIL1234.TS.NET:443": false } }),
            json!({ "TCP": { "443": tcp.clone() }, "Web": {
                HP: web.clone(), "pc.TAIL1234.ts.net:443": web.clone()
            } }),
        ];
        for config in cases {
            assert!(matches!(refused(&config), Refusal::InUse(_)), "{config}");
        }
    }

    #[test]
    fn shapes_that_decide_exposure_fail_closed() {
        let cases = [
            json!([]),
            json!({ "Unknown": {} }),
            json!({ "TCP": [] }),
            json!({ "TCP": { "https": { "HTTPS": true } } }),
            json!({ "TCP": { "8443": { "HTTPS": "yes" } } }),
            json!({ "TCP": { "8443": { "NewField": true } } }),
            json!({ "Web": { "x:8443": { "Handlers": { "/": { "NewKind": "x" } } } } }),
            json!({ "Web": { "x:8443": { "Handlers": { "/": null } } } }),
            json!({ "Web": { "x:8443": { "Extra": {} } } }),
            json!({ "AllowFunnel": { "x:8443": "true" } }),
            json!({ "Foreground": { "s": { "Foreground": {} } } }),
            json!({ "Foreground": { "s": { "Mystery": 1 } } }),
            json!({ "Foreground": [] }),
            json!({ "Services": { "svc:x": { "Mystery": 1 } } }),
            json!({ "Services": { "svc:x": { "TCP": { "1": { "Odd": 1 } } } } }),
            json!({ "Services": { "svc:x": { "Tun": "no" } } }),
            // Ports written any way but Tailscale's own are ambiguous.
            json!({ "TCP": { "0443": { "HTTPS": true } } }),
            json!({ "TCP": { "+443": { "HTTPS": true } } }),
            json!({ "TCP": { "0": { "HTTPS": true } } }),
            json!({ "TCP": { "8443": { "ProxyProtocol": 1.5 } } }),
            json!({ "TCP": { "8443": { "ProxyProtocol": 3 } } }),
            json!({ "TCP": { "8443": { "ProxyProtocol": -1 } } }),
            json!({ "Web": { "pc.tail1234.ts.net:0443": { "Handlers": {} } } }),
            json!({ "Web": { "pc.tail1234.ts.net": { "Handlers": {} } } }),
            json!({ "Web": { ":443": { "Handlers": {} } } }),
            json!({ "AllowFunnel": { "pc.tail1234.ts.net:+443": true } }),
            json!({ "Services": { "svc:x": { "Web": { "x:00443": { "Handlers": {} } } } } }),
        ];
        for config in cases {
            assert!(
                matches!(refused(&config), Refusal::Malformed(_)),
                "{config}"
            );
        }
    }

    #[test]
    fn a_missing_or_empty_version_tag_refuses() {
        for etag in [None, Some(""), Some("  ")] {
            assert_eq!(
                assess(&status(), &json!({}), etag, PORT),
                Readiness::Refused(Refusal::NoTag)
            );
        }
    }

    #[test]
    fn each_prerequisite_is_checked() {
        assert_eq!(
            refused_status(&with(status(), &["BackendState"], json!("NeedsLogin"))),
            Refusal::NotRunning("NeedsLogin".to_string())
        );
        assert_eq!(
            refused_status(&with(
                status(),
                &["CurrentTailnet", "MagicDNSEnabled"],
                json!(false)
            )),
            Refusal::NoMagicDns
        );
        assert!(matches!(
            refused_status(&with(
                status(),
                &["CurrentTailnet", "MagicDNSEnabled"],
                Value::Null
            )),
            Refusal::Malformed(_)
        ));
        assert_eq!(
            refused_status(&with(
                status(),
                &["Self", "CapMap"],
                json!({ "funnel": null })
            )),
            Refusal::NoHttps
        );
        assert_eq!(
            refused_status(&with(status(), &["Self", "CapMap"], Value::Null)),
            Refusal::NoHttps
        );
        assert_eq!(
            refused_status(&with(
                status(),
                &["CertDomains"],
                json!(["other.tail1234.ts.net"])
            )),
            Refusal::NoCertificate(NAME.to_string())
        );
        assert!(matches!(
            refused_status(&with(status(), &["Version"], json!("1.104.5-3-t73a48057a"))),
            Refusal::Unsupported(_)
        ));
        for name in [
            "pc.other.ts.net.",
            "a.pc.tail1234.ts.net.",
            "pctail1234.ts.net.",
            "tail1234.ts.net.",
            "",
        ] {
            assert!(
                matches!(
                    refused_status(&with(status(), &["Self", "DNSName"], json!(name))),
                    Refusal::NameOutsideTailnet { .. }
                ),
                "{name}"
            );
        }
        for (path, value) in [
            (&["Self", "ID"][..], Value::Null),
            (&["Self", "ID"][..], json!("")),
            (&["Self", "ID"][..], json!("  ")),
            (&["CurrentTailnet", "Name"][..], json!("")),
        ] {
            assert!(
                matches!(
                    refused_status(&with(status(), path, value)),
                    Refusal::Malformed(_)
                ),
                "{path:?}"
            );
        }
    }

    #[test]
    fn a_rename_is_blocked_by_any_use_of_the_current_name() {
        let web = json!({ "Handlers": { "/": { "Proxy": "http://127.0.0.1:3000" } } });
        for config in [
            json!({ "TCP": { "8443": { "HTTPS": true } } }),
            json!({ "TCP": { "22": { "TCPForward": "127.0.0.1:22" } } }),
            json!({ "Web": { "pc.tail1234.ts.net:8443": web.clone() } }),
            json!({ "Web": { "PC.tail1234.ts.net:10000": web.clone() } }),
            json!({ "AllowFunnel": { "pc.tail1234.ts.net:8443": false } }),
            json!({ "Foreground": { "s": { "TCP": { "9000": { "HTTPS": true } } } } }),
            json!({ "Services": { "svc:x": { "Web": { "pc.tail1234.ts.net:443": web.clone() } } } }),
        ] {
            assert!(name_in_use(&config, NAME).is_some(), "{config}");
        }
        for config in [
            Value::Null,
            json!({}),
            json!({ "Web": { "other.tail1234.ts.net:443": web.clone() } }),
            // A service under its own name is not moved by renaming the computer.
            json!({ "Services": { "svc:x": { "TCP": { "443": { "HTTPS": true } },
                "Web": { "x.tail1234.ts.net:443": web.clone() } } } }),
        ] {
            assert_eq!(name_in_use(&config, NAME), None, "{config}");
        }
    }

    #[test]
    fn the_proposed_name_is_planned_without_its_certificate() {
        // Before a rename the node's status still offers only the old name;
        // the origin plan for the new name is structural only.
        let change = ready(&json!({}));
        let planned = origin_plan(&json!({}), "desktop-k7f3q2m.tail1234.ts.net", PORT);
        let (tcp, web) = ours();
        assert_eq!(
            planned,
            Ok(OriginPlan::Free(json!({
                "TCP": { "443": tcp },
                "Web": { "desktop-k7f3q2m.tail1234.ts.net:443": web }
            })))
        );
        assert_ne!(change.config, json!({}));
    }

    #[test]
    fn an_unknown_setting_after_a_conflict_is_still_refused() {
        let config = json!({
            "Foreground": { "first": { "TCP": { "443": { "HTTPS": true } } } },
            "Services": { "svc:later": { "UnknownExposure": true } }
        });
        assert!(matches!(schema_ok(&config), Err(Refusal::Malformed(_))));
        assert!(matches!(
            origin_plan(&config, NAME, PORT),
            Err(Refusal::Malformed(_))
        ));
        // Without the unknown part it is a conflict, not a shape problem.
        let config = json!({
            "Foreground": { "first": { "TCP": { "443": { "HTTPS": true } } } }
        });
        assert_eq!(schema_ok(&config), Ok(()));
        assert!(matches!(
            origin_plan(&config, NAME, PORT),
            Err(Refusal::InUse(_))
        ));
    }

    #[test]
    fn undo_removes_exactly_the_two_entries() {
        let (tcp, web) = ours();
        let other = json!({ "Handlers": { "/": { "Proxy": "http://127.0.0.1:3000" } } });
        let config = json!({
            "TCP": { "443": tcp.clone(), "8443": { "HTTPS": true } },
            "Web": { HP: web.clone(), "pc.tail1234.ts.net:8443": other.clone() }
        });
        assert_eq!(
            origin_plan(&config, NAME, PORT),
            Ok(OriginPlan::AlreadyServed)
        );
        assert_eq!(
            without_ours(&config, NAME),
            json!({
                "TCP": { "8443": { "HTTPS": true } },
                "Web": { "pc.tail1234.ts.net:8443": other }
            })
        );
        let only = json!({ "TCP": { "443": tcp }, "Web": { HP: web } });
        assert_eq!(without_ours(&only, NAME), json!({}));
    }

    #[tokio::test]
    async fn a_write_carries_its_body_and_tag() {
        let mut response =
            b"HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\n\r\n".to_vec();
        response.extend_from_slice(b"");
        let (client, server) = fake_daemon(response);
        let payload = Payload {
            body: Some(br#"{"TCP":{}}"#.to_vec()),
            if_match: Some("tag-9".to_string()),
            token: None,
        };
        let reply = exchange(
            client,
            Method::POST,
            "/localapi/v0/serve-config",
            payload,
            soon(),
        )
        .await
        .expect("a reply");
        assert_eq!(reply.status, 412);
        let head = server.await.expect("server").to_ascii_lowercase();
        assert!(
            head.starts_with("post /localapi/v0/serve-config http/1.1\r\n"),
            "{head}"
        );
        assert!(head.contains("\r\nif-match: tag-9\r\n"), "{head}");
        assert!(
            head.contains("\r\ncontent-type: application/json\r\n"),
            "{head}"
        );
    }

    #[tokio::test]
    async fn an_unconditional_write_is_never_sent() {
        for tag in ["", "  "] {
            assert!(matches!(
                post_json("/localapi/v0/serve-config", &json!({}), tag).await,
                Err(ApiFailure::Unreachable(_))
            ));
        }
    }

    #[test]
    fn a_config_sharing_files_or_sockets_notes_the_administrator_rule() {
        for handler in [
            json!({ "Path": "/srv/files" }),
            json!({ "Proxy": "unix:/run/app.sock" }),
        ] {
            let config =
                json!({ "Web": { "pc.tail1234.ts.net:8443": { "Handlers": { "/": handler } } } });
            assert!(ready(&config).admin_note.is_some());
        }
        let config = json!({ "TCP": { "22": { "TCPForward": "unix:/run/ssh.sock" } } });
        assert!(ready(&config).admin_note.is_some());
    }

    fn soon() -> tokio::time::Instant {
        tokio::time::Instant::now() + EXCHANGE_WITHIN
    }

    /// A fake daemon on one end of an in-memory pipe: it records the request
    /// head and writes `response` back, then closes.
    fn fake_daemon(
        response: Vec<u8>,
    ) -> (tokio::io::DuplexStream, tokio::task::JoinHandle<String>) {
        daemon(response, false)
    }

    /// Like [`fake_daemon`], but after writing `response` it keeps the
    /// connection open and silent.
    fn stalling_daemon(response: Vec<u8>) -> tokio::io::DuplexStream {
        daemon(response, true).0
    }

    fn daemon(
        response: Vec<u8>,
        stall: bool,
    ) -> (tokio::io::DuplexStream, tokio::task::JoinHandle<String>) {
        let (client, mut server) = tokio::io::duplex(64 * 1024);
        let handle = tokio::spawn(async move {
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if server.read(&mut byte).await.unwrap_or(0) == 0 {
                    break;
                }
                head.push(byte[0]);
            }
            let _ = server.write_all(&response).await;
            if stall {
                std::future::pending::<()>().await;
            }
            let _ = server.shutdown().await;
            String::from_utf8_lossy(&head).into_owned()
        });
        (client, handle)
    }

    #[tokio::test(start_paused = true)]
    async fn a_body_that_stalls_after_the_head_keeps_its_status() {
        for (head, status) in [
            (
                "HTTP/1.1 403 Forbidden\r\nContent-Length: 100\r\n\r\nx",
                403,
            ),
            (
                "HTTP/1.1 200 OK\r\nEtag: t1\r\nContent-Length: 100\r\n\r\n{",
                200,
            ),
        ] {
            let client = stalling_daemon(head.as_bytes().to_vec());
            let started = tokio::time::Instant::now();
            let reply = exchange(
                client,
                Method::GET,
                "/localapi/v0/status",
                Payload::default(),
                soon(),
            )
            .await
            .expect("the head arrived, so this is a reply");
            assert_eq!(reply.status, status);
            assert!(reply.truncated);
            assert_eq!(reply.body.len(), 1);
            assert_eq!(started.elapsed(), EXCHANGE_WITHIN);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn no_head_by_the_deadline_is_interrupted() {
        let client = stalling_daemon(Vec::new());
        assert_eq!(
            exchange(
                client,
                Method::GET,
                "/localapi/v0/status",
                Payload::default(),
                soon()
            )
            .await,
            Err(ApiFailure::Interrupted("timed out".to_string()))
        );
        // A head cut off part-way is no head either.
        let client = stalling_daemon(b"HTTP/1.1 403 Forb".to_vec());
        assert!(matches!(
            exchange(
                client,
                Method::GET,
                "/localapi/v0/status",
                Payload::default(),
                soon()
            )
            .await,
            Err(ApiFailure::Interrupted(_))
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn connecting_and_the_exchange_share_one_budget() {
        let within = Duration::from_secs(5);
        // Connecting takes three seconds; the head never comes. The whole
        // request ends at five seconds, not three plus five.
        let started = tokio::time::Instant::now();
        let slow_connect = async {
            tokio::time::sleep(Duration::from_secs(3)).await;
            Ok(stalling_daemon(Vec::new()))
        };
        assert_eq!(
            request(
                slow_connect,
                Method::GET,
                "/localapi/v0/status",
                Payload::default(),
                within
            )
            .await,
            Err(ApiFailure::Interrupted("timed out".to_string()))
        );
        assert_eq!(started.elapsed(), within);

        // Connecting alone outlasts the budget: nothing was sent.
        let started = tokio::time::Instant::now();
        let stuck_connect = async {
            tokio::time::sleep(Duration::from_secs(6)).await;
            Ok(stalling_daemon(Vec::new()))
        };
        assert_eq!(
            request(
                stuck_connect,
                Method::GET,
                "/localapi/v0/status",
                Payload::default(),
                within
            )
            .await,
            Err(ApiFailure::Unreachable("connecting timed out".to_string()))
        );
        assert_eq!(started.elapsed(), within);

        // A failed connection is unreachable at once.
        let refused = async {
            Err::<tokio::io::DuplexStream, _>(ApiFailure::Unreachable("refused".to_string()))
        };
        assert_eq!(
            request(
                refused,
                Method::GET,
                "/localapi/v0/status",
                Payload::default(),
                within
            )
            .await,
            Err(ApiFailure::Unreachable("refused".to_string()))
        );
    }

    #[tokio::test]
    async fn a_request_names_the_daemon_host_and_reads_the_tag() {
        let body = br#"{"TCP":{}}"#;
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nEtag: abc123\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        let (client, server) = fake_daemon(response);
        let reply = exchange(
            client,
            Method::GET,
            "/localapi/v0/serve-config",
            Payload::default(),
            soon(),
        )
        .await
        .expect("a reply");
        assert_eq!(reply.status, 200);
        assert_eq!(reply.etag.as_deref(), Some("abc123"));
        assert_eq!(reply.body, body);
        assert!(!reply.truncated);
        let head = server.await.expect("server").to_ascii_lowercase();
        assert!(
            head.starts_with("get /localapi/v0/serve-config http/1.1\r\n"),
            "{head}"
        );
        assert!(
            head.contains("\r\nhost: local-tailscaled.sock\r\n"),
            "{head}"
        );
        assert!(
            !head.contains("origin:") && !head.contains("referer:"),
            "{head}"
        );
    }

    #[tokio::test]
    async fn a_cut_body_keeps_its_definite_status() {
        let size = MAX_REPLY + 4096;
        let mut response =
            format!("HTTP/1.1 403 Forbidden\r\nContent-Length: {size}\r\n\r\n").into_bytes();
        response.extend(std::iter::repeat(b'x').take(size));
        let (client, _server) = fake_daemon(response);
        let reply = exchange(
            client,
            Method::GET,
            "/localapi/v0/status",
            Payload::default(),
            soon(),
        )
        .await
        .expect("a reply");
        assert_eq!(reply.status, 403);
        assert!(reply.truncated);
        assert_eq!(reply.body.len(), MAX_REPLY);
    }

    #[tokio::test]
    async fn a_closed_connection_without_an_answer_is_interrupted() {
        let (client, _server) = fake_daemon(Vec::new());
        assert!(matches!(
            exchange(
                client,
                Method::GET,
                "/localapi/v0/status",
                Payload::default(),
                soon()
            )
            .await,
            Err(ApiFailure::Interrupted(_))
        ));
    }

    // ---- The Tailscale app for macOS (standalone build) ----
    // Record and token shapes are source-derived and were seen on one real
    // install (1.104.6): a root-owned `ipnport` link whose target is the port,
    // and a `sameuserproof-<port>` file holding the token. The values here are
    // placeholders.

    #[test]
    fn a_port_record_names_one_port() {
        use std::path::Path;
        assert_eq!(port_from_link(Path::new("63605")), Some(63605));
        assert_eq!(port_from_link(Path::new("1")), Some(1));
        assert_eq!(port_from_link(Path::new("65535")), Some(65535));
        for bad in [
            "", "0", "65536", "123456", "-1", "+80", " 80", "80\n", "/80", "8o",
        ] {
            assert_eq!(port_from_link(Path::new(bad)), None, "{bad:?}");
        }
    }

    #[test]
    fn a_token_is_bounded_printable_text_and_never_shown() {
        assert_eq!(
            token_from_bytes(b"placeholder-token\n"),
            Some(Token("placeholder-token".to_string()))
        );
        for bad in [
            &b""[..],
            b"\n",
            b"two words",
            b"tab\there",
            b"\xff\xfe",
            b"bell\x07",
        ] {
            assert_eq!(token_from_bytes(bad), None, "{bad:?}");
        }
        let long = vec![b'a'; usize::try_from(TOKEN_MAX).unwrap_or(256) + 1];
        assert_eq!(token_from_bytes(&long), None);

        let token = Token("placeholder-token".to_string());
        let endpoint = Endpoint::App {
            port: 63605,
            token: token.clone(),
        };
        let payload = Payload {
            token: Some(token),
            ..Payload::default()
        };
        for shown in [format!("{endpoint:?}"), format!("{payload:?}")] {
            assert!(!shown.contains("placeholder-token"), "{shown}");
            assert!(shown.contains("hidden"), "{shown}");
        }
    }

    #[cfg(unix)]
    fn app_records(port: &str, token: Option<&[u8]>) -> (tempfile::TempDir, u32) {
        use std::os::unix::fs::MetadataExt as _;
        let dir = tempfile::tempdir().expect("temp dir");
        std::os::unix::fs::symlink(port, dir.path().join("ipnport")).expect("link");
        if let Some(token) = token {
            std::fs::write(dir.path().join(format!("sameuserproof-{port}")), token).expect("token");
        }
        let owner = std::fs::metadata(dir.path()).expect("meta").uid();
        (dir, owner)
    }

    #[cfg(unix)]
    fn unreachable_text(result: Result<Endpoint, ApiFailure>) -> String {
        match result {
            Err(ApiFailure::Unreachable(why)) => why,
            other => panic!("expected unreachable, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_app_endpoint_comes_from_its_port_record_and_token() {
        let (dir, owner) = app_records("54321", Some(b"placeholder-token\n"));
        assert_eq!(
            app_endpoint(dir.path(), owner),
            Ok(Endpoint::App {
                port: 54321,
                token: Token("placeholder-token".to_string()),
            })
        );

        // Records another user owns are not the app's.
        let why = unreachable_text(app_endpoint(dir.path(), owner.wrapping_add(1)));
        assert!(why.contains("root-owned link"), "{why}");

        // No record: not running, or the App Store build.
        let empty = tempfile::tempdir().expect("temp dir");
        let why = unreachable_text(app_endpoint(empty.path(), owner));
        assert!(why.contains("App Store build"), "{why}");

        // A plain file is not the link the app writes.
        std::fs::write(empty.path().join("ipnport"), "54321").expect("file");
        let why = unreachable_text(app_endpoint(empty.path(), owner));
        assert!(why.contains("root-owned link"), "{why}");

        // A link that names no port.
        let (dir, owner) = app_records("not-a-port", None);
        let why = unreachable_text(app_endpoint(dir.path(), owner));
        assert!(why.contains("does not name a port"), "{why}");

        // A port whose token is gone: the app may be restarting.
        let (dir, owner) = app_records("54321", None);
        let why = unreachable_text(app_endpoint(dir.path(), owner));
        assert!(why.contains("missing"), "{why}");

        // A token file that is not one the app writes.
        let (dir, owner) = app_records("54321", Some(b"two words"));
        let why = unreachable_text(app_endpoint(dir.path(), owner));
        assert!(why.contains("not one the app writes"), "{why}");
        assert!(!why.contains("two words"), "{why}");
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_token_says_the_user_is_not_an_administrator() {
        use std::os::unix::fs::PermissionsExt as _;
        let (dir, owner) = app_records("54321", Some(b"placeholder-token"));
        if owner == 0 {
            return; // root reads anything; the check needs a regular user
        }
        let token = dir.path().join("sameuserproof-54321");
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let why = unreachable_text(app_endpoint(dir.path(), owner));
        assert!(why.contains("not an administrator"), "{why}");
        assert!(!why.contains("placeholder-token"), "{why}");
    }

    #[tokio::test]
    async fn the_app_request_carries_basic_credentials_and_the_daemon_host() {
        let (client, server) =
            fake_daemon(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}".to_vec());
        let payload = Payload {
            token: Some(Token("placeholder-token".to_string())),
            ..Payload::default()
        };
        let reply = exchange(client, Method::GET, "/localapi/v0/status", payload, soon())
            .await
            .expect("a reply");
        assert_eq!(reply.status, 200);
        let head = server.await.expect("server");
        let lower = head.to_ascii_lowercase();
        // base64(":placeholder-token")
        assert!(
            head.contains("\r\nauthorization: Basic OnBsYWNlaG9sZGVyLXRva2Vu\r\n"),
            "{head}"
        );
        assert!(
            lower.contains("\r\nhost: local-tailscaled.sock\r\n"),
            "{head}"
        );
        assert!(
            !lower.contains("origin:") && !lower.contains("referer:"),
            "{head}"
        );

        // The daemon's own endpoint never gets credentials.
        let (client, server) =
            fake_daemon(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec());
        exchange(
            client,
            Method::GET,
            "/localapi/v0/status",
            Payload::default(),
            soon(),
        )
        .await
        .expect("a reply");
        let head = server.await.expect("server").to_ascii_lowercase();
        assert!(!head.contains("authorization:"), "{head}");
    }

    #[test]
    fn the_app_refusing_its_token_is_unreachable_never_a_reply() {
        let app = Endpoint::App {
            port: 54321,
            token: Token("placeholder-token".to_string()),
        };
        let reply = |status| Reply {
            status,
            etag: None,
            body: b"refused".to_vec(),
            truncated: false,
        };
        // Missing or wrong credentials: refused before acting.
        assert!(matches!(
            app_admitted(&app, &reply(401), true),
            Err(ApiFailure::Unreachable(_))
        ));
        assert!(matches!(
            app_admitted(&app, &reply(401), false),
            Err(ApiFailure::Unreachable(_))
        ));
        assert!(matches!(
            app_admitted(&app, &reply(403), true),
            Err(ApiFailure::Unreachable(_))
        ));
        // A write's 403 is the caller's to report, as on every platform.
        assert_eq!(app_admitted(&app, &reply(403), false), Ok(()));
        assert_eq!(app_admitted(&app, &reply(200), true), Ok(()));
        // The daemon's own endpoint is never second-guessed.
        assert_eq!(app_admitted(&Endpoint::Daemon, &reply(401), true), Ok(()));
    }

    /// A fake app endpoint on a loopback port: it answers one request with
    /// 200 for the expected credentials, 401 without any and 403 otherwise.
    #[cfg(unix)]
    async fn fake_app(expected: &'static str) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let handle = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if socket.read(&mut byte).await.unwrap_or(0) == 0 {
                    break;
                }
                head.push(byte[0]);
            }
            let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
            let status = match head
                .lines()
                .find_map(|line| line.strip_prefix("authorization: basic "))
            {
                None => "401 Unauthorized",
                Some(given) if given == expected.to_ascii_lowercase() => "200 OK",
                Some(_) => "403 Forbidden",
            };
            let _ = socket
                .write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 2\r\n\r\n{{}}").as_bytes())
                .await;
            let _ = socket.shutdown().await;
        });
        (port, handle)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_app_endpoint_answers_only_its_own_token() {
        // base64(":placeholder-token")
        const EXPECTED: &str = "OnBsYWNlaG9sZGVyLXRva2Vu";
        for (token, admitted) in [("placeholder-token", true), ("stale-token", false)] {
            let (port, server) = fake_app(EXPECTED).await;
            let endpoint = Endpoint::App {
                port,
                token: Token(token.to_string()),
            };
            let payload = Payload {
                token: Some(Token(token.to_string())),
                ..Payload::default()
            };
            let reply = request(
                connect(&endpoint),
                Method::GET,
                "/localapi/v0/status",
                payload,
                EXCHANGE_WITHIN,
            )
            .await
            .expect("a reply");
            server.await.expect("server");
            assert_eq!(
                app_admitted(&endpoint, &reply, true).is_ok(),
                admitted,
                "{token}"
            );
        }

        // No credentials at all: 401, refused before acting.
        let (port, server) = fake_app(EXPECTED).await;
        let endpoint = Endpoint::App {
            port,
            token: Token("placeholder-token".to_string()),
        };
        let reply = request(
            connect(&endpoint),
            Method::GET,
            "/localapi/v0/status",
            Payload::default(),
            EXCHANGE_WITHIN,
        )
        .await
        .expect("a reply");
        server.await.expect("server");
        assert_eq!(reply.status, 401);
        assert!(app_admitted(&endpoint, &reply, true).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_stale_app_port_is_unreachable() {
        // Take a free port and let it go: nothing listens there any more.
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        let endpoint = Endpoint::App {
            port,
            token: Token("placeholder-token".to_string()),
        };
        match connect(&endpoint).await {
            Err(ApiFailure::Unreachable(why)) => {
                assert!(why.contains("not running"), "{why}");
                assert!(!why.contains("placeholder-token"), "{why}");
            }
            Err(other) => panic!("expected unreachable, got {other:?}"),
            Ok(_) => panic!("nothing should answer on a released port"),
        }
    }
}
