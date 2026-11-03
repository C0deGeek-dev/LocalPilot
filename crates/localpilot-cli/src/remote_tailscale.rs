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

/// Send one GET to the daemon's local API.
pub(crate) async fn get(path: &str) -> Result<Reply, ApiFailure> {
    let stream = connect().await?;
    exchange(stream, Method::GET, path).await
}

#[cfg(windows)]
async fn connect() -> Result<tokio::net::windows::named_pipe::NamedPipeClient, ApiFailure> {
    // The pipe sits under the administrators-only prefix, so whatever answers
    // there was created by the service (or an administrator), not another user.
    const PIPE: &str = r"\\.\pipe\ProtectedPrefix\Administrators\Tailscale\tailscaled";
    tokio::net::windows::named_pipe::ClientOptions::new()
        .open(PIPE)
        .map_err(|error| ApiFailure::Unreachable(unreachable_reason(&error)))
}

#[cfg(unix)]
async fn connect() -> Result<tokio::net::UnixStream, ApiFailure> {
    let path = socket_path()?;
    let connect = tokio::net::UnixStream::connect(&path);
    match tokio::time::timeout(EXCHANGE_WITHIN, connect).await {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(error)) => Err(ApiFailure::Unreachable(unreachable_reason(&error))),
        Err(_) => Err(ApiFailure::Unreachable("connecting timed out".to_string())),
    }
}

#[cfg(unix)]
fn socket_path() -> Result<std::path::PathBuf, ApiFailure> {
    if cfg!(target_os = "macos") {
        let path = std::path::PathBuf::from("/var/run/tailscaled.socket");
        if !path.exists() {
            return Err(ApiFailure::Unreachable(
                "no Tailscale daemon socket; the Tailscale app builds for macOS are not \
                 supported by setup yet, only the open-source tailscaled"
                    .to_string(),
            ));
        }
        return Ok(path);
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
async fn connect() -> Result<tokio::io::DuplexStream, ApiFailure> {
    Err(ApiFailure::Unreachable(
        "setup does not support this operating system".to_string(),
    ))
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

/// One HTTP/1.1 request over an already connected stream, bounded in time and
/// size. A connected stream that fails afterwards is [`ApiFailure::Interrupted`].
async fn exchange<S>(stream: S, method: Method, path: &str) -> Result<Reply, ApiFailure>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, API_HOST)
        .body(Full::new(Bytes::new()))
        .map_err(|error| ApiFailure::Unreachable(error.to_string()))?;
    let work = async move {
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|error| ApiFailure::Interrupted(error.to_string()))?;
        let driver = tokio::spawn(connection);
        let response = sender
            .send_request(request)
            .await
            .map_err(|error| ApiFailure::Interrupted(error.to_string()))?;
        let status = response.status().as_u16();
        let etag = response
            .headers()
            .get(header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let mut body = response.into_body();
        let mut collected = Vec::new();
        let mut truncated = false;
        while let Some(frame) = body.frame().await {
            let frame = match frame {
                Ok(frame) => frame,
                // The status already arrived: a body that breaks after it is a
                // cut reply, not an unknown outcome.
                Err(_) => {
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
    };
    match tokio::time::timeout(EXCHANGE_WITHIN, work).await {
        Ok(result) => result,
        Err(_) => Err(ApiFailure::Interrupted("timed out".to_string())),
    }
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
        node_id: text(own, "ID", "Self.")?.to_string(),
        tailnet: text(tailnet, "Name", "CurrentTailnet.")?.to_string(),
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
    /// The node and tailnet the change was planned for; the write re-checks
    /// them. Only the tests read them until the conditional write lands.
    #[cfg_attr(not(test), allow(dead_code))]
    pub node_id: String,
    #[cfg_attr(not(test), allow(dead_code))]
    pub tailnet: String,
    /// The full Serve config to write: what was read, plus the phone origin.
    /// Only the tests read it until the conditional write lands.
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
    let label = node
        .name
        .strip_suffix(&node.suffix)
        .and_then(|rest| rest.strip_suffix('.'));
    if node.suffix.is_empty()
        || !matches!(label, Some(label) if !label.is_empty() && !label.contains('.'))
    {
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
    let if_match = match etag {
        Some(tag) if !tag.trim().is_empty() => tag.to_string(),
        _ => return Err(Refusal::NoTag),
    };
    let host_port = format!("{}:{HTTPS_PORT}", node.name);
    let ours_tcp = serde_json::json!({ "HTTPS": true });
    let ours_web = serde_json::json!({
        "Handlers": { "/": { "Proxy": format!("http://127.0.0.1:{port}") } }
    });

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
                                    if key == &host_port {
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
    let origin = format!("https://{}", node.name);
    match (found.tcp, found.web) {
        (None, None) => {}
        (Some(tcp), Some(web)) if tcp == &ours_tcp && web == &ours_web => {
            return Ok(Readiness::AlreadyServed { origin });
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
    let admin_note = serves_files_or_sockets(config).then(|| {
        "your existing Serve settings share a file, folder or Unix socket, so Tailscale \
         will accept a change only from an administrator"
            .to_string()
    });
    insert(&mut top, "TCP", HTTPS_PORT.to_string(), ours_tcp);
    insert(&mut top, "Web", host_port, ours_web);
    Ok(Readiness::Ready(Change {
        origin,
        node_id: node.node_id,
        tailnet: node.tailnet,
        config: Value::Object(top),
        if_match,
        admin_note,
    }))
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
            if key == host_port {
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
            if key == host_port {
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

fn entries<'a>(value: &'a Value, at: &str) -> Result<&'a Map<String, Value>, Refusal> {
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(Refusal::Malformed(format!("{at} is not an object"))),
    }
}

fn check_tcp(value: &Value, at: &str) -> Result<(), Refusal> {
    for (port, handler) in entries(value, at)? {
        if port.parse::<u16>().is_err() {
            return Err(Refusal::Malformed(format!(
                "{at} has a non-port key {port}"
            )));
        }
        for (field, part) in object(handler, &format!("{at}.{port}"))? {
            let fits = match field.as_str() {
                "HTTPS" | "HTTP" => part.is_boolean(),
                "TCPForward" | "TerminateTLS" => part.is_string(),
                "ProxyProtocol" => part.is_number(),
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

/// Read one local API document: a complete 200 reply with a JSON body.
async fn read_document(path: &str, what: &str) -> anyhow::Result<(Value, Option<String>)> {
    let reply = get(path)
        .await
        .map_err(|failure| anyhow::anyhow!("{failure}"))?;
    match reply.status {
        200 => {}
        403 => anyhow::bail!(
            "this user may not read Tailscale's {what}; nothing was changed. On Linux, \
             `sudo tailscale set --operator=$USER` lets your user manage Tailscale."
        ),
        status => {
            let detail = String::from_utf8_lossy(&reply.body);
            let detail: String = detail.trim().chars().take(200).collect();
            anyhow::bail!("Tailscale answered {status} when asked for its {what}: {detail}");
        }
    }
    if reply.truncated {
        anyhow::bail!("Tailscale's {what} was larger than setup reads; nothing was changed");
    }
    let value = serde_json::from_slice(&reply.body)
        .map_err(|error| anyhow::anyhow!("Tailscale's {what} was not JSON: {error}"))?;
    Ok((value, reply.etag))
}

/// `localpilot remote setup --check`: say what setup would do on this
/// computer, changing nothing.
pub(crate) async fn print_check() -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let port = localpilot_config::load(
        &localpilot_config::ConfigPaths::standard(&cwd),
        &localpilot_config::CliOverrides::default(),
    )?
    .remote
    .port;
    let (status, _) = read_document("/localapi/v0/status?peers=false", "status").await?;
    let (config, etag) = read_document("/localapi/v0/serve-config", "Serve settings").await?;
    if let Ok(node) = read_node(&status) {
        println!(
            "Tailscale {} on {} (tailnet {}), {}.",
            node.version,
            if node.name.is_empty() {
                "this computer"
            } else {
                &node.name
            },
            node.tailnet,
            node.state
        );
    }
    match assess(&status, &config, etag.as_deref(), port) {
        Readiness::Ready(change) => {
            println!(
                "Setup would serve {}/ from the gateway on 127.0.0.1:{port}, changing nothing \
                 else in your Serve settings (conditional on settings version {}).",
                change.origin, change.if_match
            );
            if let Some(note) = &change.admin_note {
                println!("Note: {note}.");
            }
            println!(
                "Serving it publishes the name {} in public certificate logs.",
                change.origin.trim_start_matches("https://")
            );
            println!("This check changed nothing; applying the change is not available yet.");
            Ok(())
        }
        Readiness::AlreadyServed { origin } => {
            println!(
                "{origin}/ already forwards to the gateway on 127.0.0.1:{port}. It was not set \
                 up by LocalPilot, so LocalPilot will never remove it."
            );
            Ok(())
        }
        Readiness::Refused(refusal) => {
            anyhow::bail!("{}. {}", refusal.message(), refusal.remedy())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    // Fixtures are source-derived: shaped from the published Tailscale status
    // and Serve config types of the supported releases, not captured from a
    // real install.
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
        assert!(matches!(
            refused_status(&with(status(), &["Self", "ID"], Value::Null)),
            Refusal::Malformed(_)
        ));
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

    /// A fake daemon on one end of an in-memory pipe: it records the request
    /// head and writes `response` back.
    fn fake_daemon(
        response: Vec<u8>,
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
            let _ = server.shutdown().await;
            String::from_utf8_lossy(&head).into_owned()
        });
        (client, handle)
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
        let reply = exchange(client, Method::GET, "/localapi/v0/serve-config")
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
        let reply = exchange(client, Method::GET, "/localapi/v0/status")
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
            exchange(client, Method::GET, "/localapi/v0/status").await,
            Err(ApiFailure::Interrupted(_))
        ));
    }
}
