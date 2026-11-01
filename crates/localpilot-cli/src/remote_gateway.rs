//! `localpilot remote gateway`: the loopback HTTP and WebSocket front door
//! that lets a paired phone reach the user's `localpilot serve` workspaces.
//!
//! The gateway only carries traffic. It finds workspaces in the per-user
//! served index, registers with each server it needs (`gateway_register`, and
//! the server must confirm the workspace), and presents the browser's device
//! cookie on a fresh `device_attach`; the server decides every grant. The gateway listens
//! on loopback only: the user's own HTTPS proxy or tunnel on this computer
//! terminates TLS and forwards to it.
//!
//! What a browser request must carry is exact: one `Host` from an explicit
//! list, and, for anything that acts, one `Origin` equal to the advertised
//! origin. Forwarded headers are never trusted. Every browser frame is parsed
//! as one JSON value and re-encoded before it reaches the daemon, so a frame
//! can never become two records, and records that only the gateway or the
//! local machine may send are refused here as well as in the daemon.

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use localpilot_rpc::{
    AttachTarget, ClientCommand, ClientRecord, FeedCursor, JsonRecordReader, ServerEvent,
    ServerRecord, WireSecret, CAPABILITY_ACKS, CAPABILITY_RESUME, RPC_PROTOCOL_VERSION,
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use tokio_util::sync::CancellationToken;

/// The device credential's cookie. `__Host-` makes the browser insist on
/// `Secure`, `Path=/` and no `Domain`, so no sibling origin can set it.
pub(crate) const DEVICE_COOKIE: &str = "__Host-localpilot-device";
/// The largest record a browser may send.
const MAX_RECORD: usize = 256 * 1024;
/// The largest record the daemon may send to a browser (a snapshot of a long
/// session can be large). A larger one ends the socket; a record is never
/// split or truncated.
const MAX_OUTBOUND: usize = localpilot_rpc::DEFAULT_MAX_RECORD_LEN;
/// How often a write that has not finished re-checks the device's standing.
const ACCESS_POLL: Duration = Duration::from_millis(250);
/// The largest request body (pairing).
const MAX_BODY: usize = 4 * 1024;
/// Open TCP connections, authenticated or not.
const MAX_CONNECTIONS: usize = 64;
/// Open device sockets.
const MAX_SOCKETS: usize = 32;
/// Workspace registrations attempted at once.
const MAX_REGISTRATIONS: usize = 4;
/// How long a client may take to send its request headers.
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a request body or the daemon's first answer may take.
const STEP_TIMEOUT: Duration = Duration::from_secs(10);
/// The device cookie's lifetime: the browser maximum. Revocation, not expiry,
/// is what ends a device's access.
const COOKIE_MAX_AGE_SECS: u64 = 400 * 24 * 60 * 60;

/// Records a browser must never send: binding a connection, or registering
/// and pairing, belong to the gateway and the local machine.
const FORBIDDEN_RECORDS: [&str; 5] = [
    "attach",
    "gateway_register",
    "device_attach",
    "pair_begin",
    "pair_exchange",
];

/// A connection to the daemon.
pub(crate) trait DaemonStream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> DaemonStream for T {}

/// What a browser request must carry.
#[derive(Debug, Clone)]
pub(crate) struct Policy {
    /// The advertised origin, e.g. `https://pc.example.ts.net`.
    origin: String,
    /// The exact `Host` values the proxy sends, lower-cased.
    pub(crate) hosts: Vec<String>,
}

impl Policy {
    /// `hosts` empty means the advertised origin's own host.
    pub(crate) fn new(origin: &str, hosts: &[String]) -> Self {
        let hosts = if hosts.is_empty() {
            vec![origin.trim_start_matches("https://").to_ascii_lowercase()]
        } else {
            hosts.iter().map(|host| host.to_ascii_lowercase()).collect()
        };
        Self {
            origin: origin.to_string(),
            hosts,
        }
    }

    /// Exactly one `Host`, and it is one of the allowed values.
    fn host_ok(&self, headers: &HeaderMap) -> bool {
        single(headers, &header::HOST).is_some_and(|host| {
            self.hosts
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(host))
        })
    }

    /// The host check, plus exactly one `Origin` equal to the advertised one.
    fn origin_ok(&self, headers: &HeaderMap) -> bool {
        self.host_ok(headers) && single(headers, &header::ORIGIN) == Some(self.origin.as_str())
    }
}

/// The value of a header that must appear exactly once.
fn single<'a>(headers: &'a HeaderMap, name: &header::HeaderName) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    value.to_str().ok()
}

/// The device cookie, when the request carries it exactly once.
fn device_cookie(headers: &HeaderMap) -> Option<String> {
    let mut found = None;
    for value in headers.get_all(header::COOKIE) {
        let value = value.to_str().ok()?;
        for pair in value.split(';') {
            let Some((name, content)) = pair.trim().split_once('=') else {
                continue;
            };
            if name == DEVICE_COOKIE {
                if found.is_some() {
                    return None;
                }
                found = Some(content.to_string());
            }
        }
    }
    found.filter(|content| !content.is_empty())
}

/// A `Sec-WebSocket-Key` as RFC 6455 requires: the canonical base64 of
/// exactly 16 bytes (22 symbols, the last carrying no stray bits, then `==`).
fn valid_websocket_key(key: &str) -> bool {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = key.as_bytes();
    if bytes.len() != 24 || &bytes[22..] != b"==" {
        return false;
    }
    let mut last = 0;
    for &b in &bytes[..22] {
        match ALPHABET.iter().position(|&a| a == b) {
            Some(value) => last = value,
            None => return false,
        }
    }
    // 16 bytes are 128 bits; 22 symbols carry 132, so the last symbol's low
    // four bits must be zero.
    last & 0b1111 == 0
}

/// Why a browser frame was not passed on.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FrameRefusal {
    TooLarge,
    NotJson,
    Forbidden,
}

/// One browser frame as exactly one daemon record: parsed, checked, and
/// re-encoded on a single line.
pub(crate) fn frame_to_record(text: &str) -> Result<String, FrameRefusal> {
    if text.len() > MAX_RECORD {
        return Err(FrameRefusal::TooLarge);
    }
    let value: serde_json::Value = serde_json::from_str(text).map_err(|_| FrameRefusal::NotJson)?;
    if !value.is_object() {
        return Err(FrameRefusal::NotJson);
    }
    let kind = value
        .get("command")
        .and_then(|command| command.get("type"))
        .and_then(serde_json::Value::as_str);
    if kind.is_some_and(|kind| FORBIDDEN_RECORDS.contains(&kind)) {
        return Err(FrameRefusal::Forbidden);
    }
    // `to_string` escapes every control character, so the line holds one
    // record whatever the frame contained.
    let mut line = serde_json::to_string(&value).map_err(|_| FrameRefusal::NotJson)?;
    line.push('\n');
    Ok(line)
}

/// What a socket asks for, from its query: the workspace (`workspace=<id>`),
/// and `session=<id>` to resume a session or `new=1` to open one; `epoch` and
/// `seq` resume a feed position. Every key appears at most once.
#[derive(Debug)]
struct SocketQuery {
    workspace: String,
    target: AttachTarget,
    since: Option<FeedCursor>,
}

fn socket_query(query: Option<&str>) -> Option<SocketQuery> {
    let mut workspace = None;
    let mut session = None;
    let mut new = None;
    let mut epoch = None;
    let mut seq = None;
    for pair in query.unwrap_or_default().split('&') {
        let (key, value) = pair.split_once('=')?;
        let slot = match key {
            "workspace" => &mut workspace,
            "session" => &mut session,
            "new" => &mut new,
            "epoch" => &mut epoch,
            "seq" => &mut seq,
            _ => return None,
        };
        if slot.replace(value.to_string()).is_some() {
            return None; // a key given twice is ambiguous
        }
    }
    let workspace = workspace.filter(|id| crate::server_index::is_discovery_id(id))?;
    let target = match (session, new.as_deref()) {
        (Some(id), None) => AttachTarget::ResumeId {
            session_id: id.parse().ok()?,
        },
        (None, Some("1")) => AttachTarget::OpenNew,
        _ => return None,
    };
    let since = match (epoch, seq) {
        (Some(epoch), Some(seq)) => Some(FeedCursor {
            epoch,
            seq: seq.parse().ok()?,
        }),
        (None, None) => None,
        _ => return None,
    };
    Some(SocketQuery {
        workspace,
        target,
        since,
    })
}

/// A served workspace, as the gateway knows it: its discovery id, a display
/// label, and its root (used only to derive its server's endpoint and to check
/// a device's grant; never sent to a device).
#[derive(Debug, Clone)]
pub(crate) struct Workspace {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) root: std::path::PathBuf,
}

/// Opens a new connection to a workspace's server.
pub(crate) type Dial = Arc<
    dyn Fn(
            &Workspace,
        ) -> Pin<Box<dyn Future<Output = std::io::Result<Box<dyn DaemonStream>>> + Send>>
        + Send
        + Sync,
>;

/// A credential's device id, if the credential is valid and not revoked.
pub(crate) type Verify = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;
/// Whether a device may currently reach the workspace at a root.
pub(crate) type Reach = Arc<dyn Fn(&str, &std::path::Path) -> bool + Send + Sync>;

/// How the gateway finds workspaces and checks devices. The servers remain
/// the authority on every grant; the gateway uses `verify` and `standing`
/// only to decide what to list and when to stop forwarding.
#[derive(Clone)]
pub(crate) struct Discovery {
    /// The published workspaces.
    pub(crate) list: Arc<dyn Fn() -> Vec<Workspace> + Send + Sync>,
    pub(crate) dial: Dial,
    /// A credential's device id, if the credential is valid and not revoked.
    pub(crate) verify: Verify,
    /// Whether the device may currently reach the workspace at `root`.
    pub(crate) standing: Reach,
}

/// A workspace's registration: its capability, valid while the control
/// channel lives, and the token that ends with it.
#[derive(Clone)]
struct Live {
    capability: WireSecret,
    token: CancellationToken,
}

/// The gateway: its policy, how it finds workspaces, one registration per
/// workspace, and the token that stops everything.
pub(crate) struct Gateway {
    policy: Policy,
    discovery: Discovery,
    stop: CancellationToken,
    connections: Arc<Semaphore>,
    sockets: Arc<Semaphore>,
    /// Registrations in progress at once.
    registrations: Arc<Semaphore>,
    /// Per workspace: its live registration, behind a lock so only one
    /// registration is attempted at a time.
    live: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<Option<Live>>>>>,
    /// The deadline for each step that waits on a server or a client.
    step: Duration,
}

impl Gateway {
    pub(crate) fn new(policy: Policy, discovery: Discovery, stop: CancellationToken) -> Arc<Self> {
        Self::with_step(policy, discovery, stop, STEP_TIMEOUT)
    }

    /// As [`Gateway::new`], with `step` as the deadline for each wait.
    pub(crate) fn with_step(
        policy: Policy,
        discovery: Discovery,
        stop: CancellationToken,
        step: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            policy,
            discovery,
            stop,
            connections: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            sockets: Arc::new(Semaphore::new(MAX_SOCKETS)),
            registrations: Arc::new(Semaphore::new(MAX_REGISTRATIONS)),
            live: std::sync::Mutex::new(HashMap::new()),
            step,
        })
    }

    /// The published workspace with this id.
    fn workspace(&self, id: &str) -> Option<Workspace> {
        (self.discovery.list)()
            .into_iter()
            .find(|workspace| workspace.id == id)
    }

    /// The workspace's live registration, registering with its server first
    /// if needed. The server must confirm it serves this workspace; a server
    /// that names another workspace, or none, is treated as offline. One
    /// workspace's control channel ending ends only that workspace's sockets.
    ///
    /// Waiting for another caller's registration, for a registration slot and
    /// for the server all count against one deadline, so a server that does
    /// not answer delays each request by at most one step.
    async fn reach(&self, workspace: &Workspace) -> Option<Live> {
        tokio::select! {
            () = self.stop.cancelled() => None,
            reached = tokio::time::timeout(self.step, self.reach_now(workspace)) => {
                reached.ok().flatten()
            }
        }
    }

    async fn reach_now(&self, workspace: &Workspace) -> Option<Live> {
        let slot = self
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(workspace.id.clone())
            .or_default()
            .clone();
        let mut held = slot.lock().await;
        if let Some(live) = held.as_ref().filter(|live| !live.token.is_cancelled()) {
            return Some(live.clone());
        }
        *held = None;
        let registered = async {
            let _permit = self.registrations.acquire().await.ok()?;
            let control = (self.discovery.dial)(workspace).await.ok()?;
            let (read, mut write) = tokio::io::split(control);
            let mut reader = JsonRecordReader::new(read);
            write_record(&mut write, ClientCommand::GatewayRegister)
                .await
                .ok()?;
            match next_event(&mut reader).await.ok()?? {
                ServerEvent::GatewayRegistered {
                    capability,
                    workspace: Some(confirmed),
                } if confirmed == workspace.id => Some((capability, reader, write)),
                _ => None,
            }
        };
        let (capability, mut reader, write) = registered.await?;
        let token = self.stop.child_token();
        tokio::spawn({
            let token = token.clone();
            async move {
                tokio::select! {
                    () = token.cancelled() => {}
                    _ = async { while let Ok(Some(_)) = reader.next().await {} } => token.cancel(),
                }
                drop(write);
            }
        });
        let live = Live { capability, token };
        *held = Some(live.clone());
        Some(live)
    }

    /// Accept connections until the gateway stops.
    pub(crate) async fn serve(self: Arc<Self>, listener: TcpListener) {
        loop {
            let accepted = tokio::select! {
                () = self.stop.cancelled() => return,
                accepted = listener.accept() => accepted,
            };
            let Ok((stream, _)) = accepted else { continue };
            // Over the cap, the connection is closed before it is read.
            let Ok(permit) = self.connections.clone().try_acquire_owned() else {
                continue;
            };
            let gateway = self.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn({
                    let gateway = gateway.clone();
                    move |request| {
                        let gateway = gateway.clone();
                        async move { Ok::<_, Infallible>(gateway.route(request).await) }
                    }
                });
                let connection = hyper::server::conn::http1::Builder::new()
                    .timer(TokioTimer::new())
                    .header_read_timeout(HEADER_TIMEOUT)
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades();
                tokio::select! {
                    () = gateway.stop.cancelled() => {}
                    _ = connection => {}
                }
                drop(permit);
            });
        }
    }

    async fn route(self: Arc<Self>, request: Request<Incoming>) -> Response<Full<Bytes>> {
        if !self.policy.host_ok(request.headers()) {
            return plain(StatusCode::MISDIRECTED_REQUEST, "unknown host");
        }
        match (request.method(), request.uri().path()) {
            (&Method::GET, "/") => page(),
            (&Method::GET, "/healthz") => plain(StatusCode::OK, &health_body()),
            (&Method::GET, "/api/workspaces") => self.workspaces(&request).await,
            (&Method::POST, "/api/pair") => self.pair(request).await,
            (&Method::GET, "/api/socket") => {
                // A refused upgrade closes its connection rather than waiting
                // for another request.
                let mut response = self.socket(request).await;
                if response.status() != StatusCode::SWITCHING_PROTOCOLS {
                    response
                        .headers_mut()
                        .insert(header::CONNECTION, HeaderValue::from_static("close"));
                }
                response
            }
            _ => plain(StatusCode::NOT_FOUND, "not found"),
        }
    }

    /// The workspaces this device may reach, with whether each is online.
    /// Read-only; the credential is verified and each workspace's grant
    /// checked before anything about it is returned or probed.
    async fn workspaces(&self, request: &Request<Incoming>) -> Response<Full<Bytes>> {
        if !self.policy.origin_ok(request.headers()) {
            return plain(StatusCode::FORBIDDEN, "forbidden");
        }
        let Some(device) = device_cookie(request.headers())
            .and_then(|credential| (self.discovery.verify)(&credential))
        else {
            return plain(StatusCode::UNAUTHORIZED, "this device is not paired");
        };
        let allowed: Vec<Workspace> = (self.discovery.list)()
            .into_iter()
            .filter(|workspace| (self.discovery.standing)(&device, &workspace.root))
            .collect();
        let probes = allowed.iter().map(|workspace| self.reach(workspace));
        let online = futures::future::join_all(probes).await;
        let items: Vec<serde_json::Value> = allowed
            .iter()
            .zip(online)
            .map(|(workspace, live)| {
                serde_json::json!({
                    "id": workspace.id,
                    "label": workspace.label,
                    "online": live.is_some(),
                })
            })
            .collect();
        json(&serde_json::json!({ "workspaces": items }))
    }

    /// Exchange a pairing code (from the QR link's fragment, sent in the body)
    /// for a device credential, which goes only into the device cookie. The
    /// code is presented only to the workspace that issued it.
    async fn pair(&self, request: Request<Incoming>) -> Response<Full<Bytes>> {
        if !self.policy.origin_ok(request.headers()) {
            return plain(StatusCode::FORBIDDEN, "forbidden");
        }
        let is_json = single(request.headers(), &header::CONTENT_TYPE)
            .is_some_and(|value| value.starts_with("application/json"));
        if !is_json {
            return plain(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "expected application/json",
            );
        }
        let body = Limited::new(request.into_body(), MAX_BODY).collect();
        let Ok(Ok(body)) = tokio::time::timeout(self.step, body).await else {
            return plain(StatusCode::BAD_REQUEST, "unreadable request");
        };
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Pairing {
            code: String,
            workspace: String,
        }
        let Ok(Pairing { code, workspace }) = serde_json::from_slice::<Pairing>(&body.to_bytes())
        else {
            return plain(
                StatusCode::BAD_REQUEST,
                "expected {\"code\": ..., \"workspace\": ...}",
            );
        };
        if !crate::server_index::is_discovery_id(&workspace) {
            return plain(StatusCode::BAD_REQUEST, "not a workspace id");
        }
        let Some(workspace) = self.workspace(&workspace) else {
            return plain(StatusCode::NOT_FOUND, "that workspace is not being served");
        };
        let Some(live) = self.reach(&workspace).await else {
            return plain(StatusCode::SERVICE_UNAVAILABLE, "that workspace is offline");
        };
        let exchanged = self
            .ask_daemon(
                &workspace,
                &live,
                ClientCommand::PairExchange {
                    capability: live.capability.clone(),
                    pairing: WireSecret::new(code),
                },
            )
            .await;
        let Ok((
            Some(ServerEvent::DevicePaired {
                device_id,
                credential,
                scope,
            }),
            _,
        )) = exchanged
        else {
            return plain(StatusCode::FORBIDDEN, "the pairing code was not accepted");
        };
        let credential = credential.expose();
        if !credential
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.')
        {
            return plain(StatusCode::INTERNAL_SERVER_ERROR, "unusable credential");
        }
        let mut response = json(&serde_json::json!({ "device_id": device_id, "scope": scope }));
        if let Ok(cookie) = HeaderValue::from_str(&format!(
            "{DEVICE_COOKIE}={credential}; Path=/; Secure; HttpOnly; SameSite=Strict; \
             Max-Age={COOKIE_MAX_AGE_SECS}"
        )) {
            response.headers_mut().insert(header::SET_COOKIE, cookie);
        }
        response
    }

    /// Open a device socket: the daemon accepts the device's `device_attach`
    /// before the upgrade is answered, and nothing from the browser reaches
    /// the daemon before that.
    async fn socket(&self, mut request: Request<Incoming>) -> Response<Full<Bytes>> {
        let headers = request.headers();
        if !self.policy.origin_ok(headers) {
            return plain(StatusCode::FORBIDDEN, "forbidden");
        }
        let http11 = request.version() == hyper::Version::HTTP_11;
        let upgrade = single(headers, &header::UPGRADE)
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
        let connection = headers
            .get_all(header::CONNECTION)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .any(|token| token.trim().eq_ignore_ascii_case("upgrade"));
        let version = single(headers, &header::SEC_WEBSOCKET_VERSION) == Some("13");
        let key =
            single(headers, &header::SEC_WEBSOCKET_KEY).filter(|key| valid_websocket_key(key));
        // The whole handshake is checked before anything reaches the daemon,
        // so an invalid upgrade never opens or resumes a session.
        let (true, true, true, true, Some(key)) = (http11, upgrade, connection, version, key)
        else {
            return plain(StatusCode::BAD_REQUEST, "expected a WebSocket upgrade");
        };
        let accept = derive_accept_key(key.as_bytes());
        let Some(credential) = device_cookie(headers) else {
            return plain(StatusCode::UNAUTHORIZED, "this device is not paired");
        };
        let Some(device) =
            crate::server_devices::credential_device_id(&credential).map(str::to_string)
        else {
            return plain(StatusCode::UNAUTHORIZED, "this device is not paired");
        };
        let Some(query) = socket_query(request.uri().query()) else {
            return plain(
                StatusCode::BAD_REQUEST,
                "choose a workspace and a session: ?workspace=<id>&session=<id> or &new=1",
            );
        };
        let Some(workspace) = self.workspace(&query.workspace) else {
            return plain(StatusCode::NOT_FOUND, "that workspace is not being served");
        };
        if !(self.discovery.standing)(&device, &workspace.root) {
            return plain(
                StatusCode::FORBIDDEN,
                "this device may not reach that workspace",
            );
        }
        let Ok(permit) = self.sockets.clone().try_acquire_owned() else {
            return plain(StatusCode::SERVICE_UNAVAILABLE, "too many open devices");
        };
        let Some(live) = self.reach(&workspace).await else {
            return plain(StatusCode::SERVICE_UNAVAILABLE, "that workspace is offline");
        };
        let attached = self
            .ask_daemon(
                &workspace,
                &live,
                ClientCommand::DeviceAttach {
                    capability: live.capability.clone(),
                    credential: WireSecret::new(credential),
                    target: query.target,
                    capabilities: vec![CAPABILITY_ACKS.to_string(), CAPABILITY_RESUME.to_string()],
                    since: query.since,
                },
            )
            .await;
        let (first, daemon) = match attached {
            Ok((Some(ServerEvent::Attached { .. }), Some((daemon, record)))) => (record, daemon),
            Ok((Some(ServerEvent::Error { message }), _)) => {
                return plain(StatusCode::FORBIDDEN, &message);
            }
            _ => {
                return plain(
                    StatusCode::BAD_GATEWAY,
                    "the server did not accept the device",
                )
            }
        };
        let upgraded = hyper::upgrade::on(&mut request);
        // The socket ends with its workspace's registration (and with it the
        // whole gateway).
        let stop = live.token.clone();
        let step = self.step;
        let standing = self.discovery.standing.clone();
        let root = workspace.root.clone();
        let access: Access = Arc::new(move || standing(&device, &root));
        tokio::spawn(async move {
            let upgraded = tokio::select! {
                () = stop.cancelled() => return,
                upgraded = tokio::time::timeout(step, upgraded) => upgraded,
            };
            let Ok(Ok(upgraded)) = upgraded else { return };
            relay(TokioIo::new(upgraded), daemon, first, stop, access, permit).await;
        });
        let mut response = with_headers(Response::new(Full::new(Bytes::new())));
        *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
        let headers = response.headers_mut();
        headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        headers.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
        if let Ok(accept) = HeaderValue::from_str(&accept) {
            headers.insert(header::SEC_WEBSOCKET_ACCEPT, accept);
        }
        response
    }

    /// Send one first record on a new connection to the workspace's server and
    /// read its answer. Returns the answer, and the open connection with the
    /// raw answer.
    async fn ask_daemon(
        &self,
        workspace: &Workspace,
        live: &Live,
        command: ClientCommand,
    ) -> anyhow::Result<(Option<ServerEvent>, Option<(Daemon, serde_json::Value)>)> {
        let step = async {
            let stream = (self.discovery.dial)(workspace).await?;
            let (read, mut write) = tokio::io::split(stream);
            let mut reader = JsonRecordReader::new(read);
            write_record(&mut write, command).await?;
            let Some(value) = reader.next().await? else {
                return Ok((None, None));
            };
            let event = serde_json::from_value::<ServerRecord>(value.clone())
                .ok()
                .map(|record| record.event);
            Ok((event, Some(((reader, write), value))))
        };
        tokio::select! {
            () = live.token.cancelled() => anyhow::bail!("the workspace's registration ended"),
            answered = tokio::time::timeout(self.step, step) => answered?,
        }
    }
}

/// An attached daemon connection: its record reader and its write half.
type Daemon = (
    JsonRecordReader<tokio::io::ReadHalf<Box<dyn DaemonStream>>>,
    tokio::io::WriteHalf<Box<dyn DaemonStream>>,
);

/// Whether this socket's device still has access.
type Access = Arc<dyn Fn() -> bool + Send + Sync>;

/// Run one write while the gateway runs and the device keeps its access:
/// `None` as soon as either ends. Access is checked before the write starts
/// and every [`ACCESS_POLL`] while it is blocked.
async fn guarded<T>(
    stop: &CancellationToken,
    access: &Access,
    write: impl Future<Output = T>,
) -> Option<T> {
    if stop.is_cancelled() || !access() {
        return None;
    }
    let lost = async {
        let mut poll =
            tokio::time::interval_at(tokio::time::Instant::now() + ACCESS_POLL, ACCESS_POLL);
        loop {
            poll.tick().await;
            if !access() {
                return;
            }
        }
    };
    tokio::select! {
        biased;
        () = stop.cancelled() => None,
        () = lost => None,
        done = write => Some(done),
    }
}

/// Carry records between an upgraded browser socket and its daemon
/// connection until either ends, the gateway stops, or the device loses
/// access. Every write is guarded, so a stalled browser or daemon never keeps
/// a revoked device's record in flight: on loss the socket is dropped with
/// whatever it held.
async fn relay<S: AsyncRead + AsyncWrite + Unpin>(
    socket: S,
    daemon: Daemon,
    first: serde_json::Value,
    stop: CancellationToken,
    access: Access,
    _permit: OwnedSemaphorePermit,
) {
    let config = WebSocketConfig {
        max_message_size: Some(MAX_RECORD),
        max_frame_size: Some(MAX_RECORD),
        ..WebSocketConfig::default()
    };
    let ws = WebSocketStream::from_raw_socket(socket, Role::Server, Some(config)).await;
    let (mut to_browser, mut from_browser) = ws.split();
    let (mut reader, mut to_daemon) = daemon;
    let mut outgoing = Some(first);
    loop {
        if let Some(value) = outgoing.take() {
            let text = value.to_string();
            if text.len() > MAX_OUTBOUND {
                return;
            }
            match guarded(&stop, &access, to_browser.send(Message::text(text))).await {
                Some(Ok(())) => {}
                _ => return,
            }
        }
        tokio::select! {
            biased;
            () = stop.cancelled() => return,
            record = reader.next() => match record {
                Ok(Some(value)) => outgoing = Some(value),
                _ => return, // the daemon ended the device's connection
            },
            frame = from_browser.next() => match frame {
                Some(Ok(Message::Text(text))) => {
                    let Ok(line) = frame_to_record(&text) else { return };
                    let write = async {
                        to_daemon.write_all(line.as_bytes()).await?;
                        to_daemon.flush().await
                    };
                    match guarded(&stop, &access, write).await {
                        Some(Ok(())) => {}
                        _ => return,
                    }
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                // Binary frames, a close, or a broken socket end it.
                _ => return,
            },
        }
    }
}

async fn write_record<W: AsyncWrite + Unpin>(
    write: &mut W,
    command: ClientCommand,
) -> std::io::Result<()> {
    let record = ClientRecord {
        v: RPC_PROTOCOL_VERSION,
        id: None,
        request: None,
        command,
    };
    let mut line = serde_json::to_vec(&record)?;
    line.push(b'\n');
    write.write_all(&line).await?;
    write.flush().await
}

async fn next_event<R: AsyncRead + Unpin>(
    reader: &mut JsonRecordReader<R>,
) -> anyhow::Result<Option<ServerEvent>> {
    let Some(value) = tokio::time::timeout(STEP_TIMEOUT, reader.next()).await?? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_value::<ServerRecord>(value)?.event))
}

/// Headers every gateway response carries.
fn with_headers(mut response: Response<Full<Bytes>>) -> Response<Full<Bytes>> {
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; connect-src 'self'; img-src 'self'; style-src 'self'; \
             frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// The body `/healthz` answers with, which `localpilot remote status` checks.
pub(crate) fn health_body() -> String {
    format!("localpilot-gateway {}", env!("CARGO_PKG_VERSION"))
}

fn json(value: &serde_json::Value) -> Response<Full<Bytes>> {
    let mut response = with_headers(Response::new(Full::new(Bytes::from(value.to_string()))));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

fn plain(status: StatusCode, text: &str) -> Response<Full<Bytes>> {
    let mut response = with_headers(Response::new(Full::new(Bytes::from(text.to_string()))));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

/// The page served at `/` until the companion app ships with the gateway.
fn page() -> Response<Full<Bytes>> {
    const PAGE: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
        <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
        <title>LocalPilot</title></head><body><h1>LocalPilot gateway</h1>\
        <p>The gateway is running. The companion app is not included in this \
        build yet.</p></body></html>";
    let mut response = with_headers(Response::new(Full::new(Bytes::from_static(
        PAGE.as_bytes(),
    ))));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response
}

/// The gateway's view of this user's served workspaces and devices.
fn user_discovery() -> anyhow::Result<Discovery> {
    let Some(index) = crate::server_index::ServedIndex::for_user() else {
        anyhow::bail!("no home directory is set, so there is no per-user workspace index");
    };
    let list: Arc<dyn Fn() -> Vec<Workspace> + Send + Sync> = Arc::new(move || {
        index
            .list()
            .into_iter()
            .map(|entry| Workspace {
                id: entry.id,
                label: entry.label,
                root: entry.root,
            })
            .collect()
    });
    // Always the workspace's own default endpoint: an endpoint override in the
    // gateway's environment must never send every workspace to one server.
    let dial: Dial = Arc::new(|workspace: &Workspace| {
        let endpoint = localpilot_server::Endpoint::for_workspace(&workspace.root);
        Box::pin(async move {
            let endpoint = endpoint.map_err(std::io::Error::other)?;
            localpilot_server::connect(&endpoint)
                .await
                .map(|conn| Box::new(conn) as Box<dyn DaemonStream>)
                .map_err(std::io::Error::other)
        })
    });
    // Only reach-free verification and per-root standing are used, so the
    // workspace the registry is opened for does not matter.
    let registry = Arc::new(crate::server_devices::DeviceRegistry::for_user(
        &std::env::current_dir()?,
    ));
    let verify = {
        let registry = registry.clone();
        Arc::new(move |credential: &str| registry.verify_device(credential).ok())
    };
    let standing = Arc::new(move |device: &str, root: &std::path::Path| {
        registry.standing_in(device, root).is_ok()
    });
    Ok(Discovery {
        list,
        dial,
        verify,
        standing,
    })
}

/// Whether the gateway answers on its loopback port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GatewayHealth {
    Running,
    NotRunning,
    /// Something answers on the port, but not this gateway.
    Unexpected,
}

/// The `[remote]` settings as `remote status` reports them.
#[derive(Debug, Clone)]
pub(crate) struct RemoteSettings {
    pub(crate) enabled: bool,
    /// The advertised origin; `Err` names why the configured URL is invalid.
    pub(crate) advertised: Result<Option<String>, String>,
    pub(crate) port: u16,
}

/// What `localpilot remote status` shows. No secrets. A part that cannot be
/// read is reported with its reason, never as an empty or default value.
#[derive(Debug, Clone)]
pub(crate) struct RemoteStatus {
    pub(crate) settings: Result<RemoteSettings, String>,
    /// `None` when the settings, and so the port, are unknown.
    pub(crate) gateway: Option<GatewayHealth>,
    /// Workspaces served with remote access on: label and discovery id.
    pub(crate) workspaces: Vec<(String, String)>,
    /// Paired and revoked device counts.
    pub(crate) devices: Result<(usize, usize), String>,
}

/// Ask the loopback port for the gateway's health answer.
async fn probe(port: u16, host: &str) -> GatewayHealth {
    use tokio::io::AsyncReadExt as _;
    let exchange = async {
        let mut socket =
            tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).await?;
        socket
            .write_all(
                format!("GET /healthz HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await?;
        let mut raw = Vec::new();
        socket.read_to_end(&mut raw).await?;
        Ok::<_, std::io::Error>(String::from_utf8_lossy(&raw).into_owned())
    };
    match tokio::time::timeout(Duration::from_secs(2), exchange).await {
        Ok(Ok(text)) => {
            let ok = text.starts_with("HTTP/1.1 200")
                && text.split_once("\r\n\r\n").map(|(_, body)| body) == Some(&health_body());
            if ok {
                GatewayHealth::Running
            } else {
                GatewayHealth::Unexpected
            }
        }
        Ok(Err(_)) => GatewayHealth::NotRunning,
        Err(_) => GatewayHealth::Unexpected,
    }
}

/// The settings part of the status, from a loaded (or failed) config.
fn settings_of(
    config: Result<localpilot_config::RemoteConfig, String>,
) -> Result<(RemoteSettings, Vec<String>), String> {
    let config = config?;
    let advertised = config
        .advertised_url
        .as_deref()
        .map(|url| crate::server_cmd::pairing_origin(url).map_err(|error| error.to_string()))
        .transpose();
    Ok((
        RemoteSettings {
            enabled: config.enabled,
            advertised,
            port: config.port,
        },
        config.proxy_hosts,
    ))
}

/// The device counts, or why the registry cannot be read.
fn device_counts(
    registry: &crate::server_devices::DeviceRegistry,
) -> Result<(usize, usize), String> {
    let devices = registry.list().map_err(|error| error.to_string())?;
    let revoked = devices
        .iter()
        .filter(|device| device.revoked_unix.is_some())
        .count();
    Ok((devices.len() - revoked, revoked))
}

/// Gather the status from its sources.
async fn status_from(
    config: Result<localpilot_config::RemoteConfig, String>,
    index: Option<&crate::server_index::ServedIndex>,
    registry: &crate::server_devices::DeviceRegistry,
) -> RemoteStatus {
    let settings = settings_of(config);
    let gateway = match &settings {
        Ok((settings, proxy_hosts)) => {
            let host = match &settings.advertised {
                Ok(Some(origin)) => Policy::new(origin, proxy_hosts).hosts.first().cloned(),
                _ => None,
            }
            .unwrap_or_else(|| "localhost".to_string());
            Some(probe(settings.port, &host).await)
        }
        Err(_) => None,
    };
    let workspaces = index
        .map(crate::server_index::ServedIndex::list)
        .unwrap_or_default()
        .into_iter()
        .map(|entry| (entry.label, entry.id))
        .collect();
    RemoteStatus {
        settings: settings.map(|(settings, _)| settings),
        gateway,
        workspaces,
        devices: device_counts(registry),
    }
}

/// Gather the remote-access status for this user.
pub(crate) async fn status() -> RemoteStatus {
    let cwd = std::env::current_dir().unwrap_or_default();
    let config = localpilot_config::load(
        &localpilot_config::ConfigPaths::standard(&cwd),
        &localpilot_config::CliOverrides::default(),
    )
    .map(|config| config.remote)
    .map_err(|error| error.to_string());
    let index = crate::server_index::ServedIndex::for_user();
    let registry = crate::server_devices::DeviceRegistry::for_user(&cwd);
    status_from(config, index.as_ref(), &registry).await
}

/// The status as indented text lines.
pub(crate) fn render_status(status: &RemoteStatus) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    match &status.settings {
        Ok(settings) => {
            let enabled = if settings.enabled {
                "yes"
            } else {
                "no ([remote] enabled = false)"
            };
            let _ = writeln!(s, "  enabled:        {enabled}");
            let advertised = match &settings.advertised {
                Ok(Some(origin)) => origin.clone(),
                Ok(None) => "(not set)".to_string(),
                Err(reason) => format!("invalid: {reason}"),
            };
            let _ = writeln!(s, "  advertised url: {advertised}");
            let gateway = match status.gateway {
                Some(GatewayHealth::Running) => "running",
                Some(GatewayHealth::NotRunning) => "not running",
                Some(GatewayHealth::Unexpected) => "port in use by something else",
                None => "unknown",
            };
            let _ = writeln!(
                s,
                "  gateway:        {gateway} (127.0.0.1:{})",
                settings.port
            );
        }
        Err(reason) => {
            let _ = writeln!(s, "  settings:       cannot be read: {reason}");
            let _ = writeln!(s, "  gateway:        unknown (no port without settings)");
        }
    }
    if status.workspaces.is_empty() {
        let _ = writeln!(s, "  workspaces:     none served with remote access on");
    } else {
        let _ = writeln!(s, "  workspaces:");
        for (label, id) in &status.workspaces {
            let _ = writeln!(s, "    {label} ({id})");
        }
    }
    match &status.devices {
        Ok((active, revoked)) => {
            let _ = writeln!(s, "  devices:        {active} paired, {revoked} revoked");
        }
        Err(reason) => {
            let _ = writeln!(s, "  devices:        cannot be read: {reason}");
        }
    }
    s
}

/// `localpilot remote status`.
pub(crate) async fn print_status() {
    print!("remote access:\n{}", render_status(&status().await));
}

/// `localpilot remote gateway`: run until interrupted or switched off.
pub(crate) async fn run() -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let config = localpilot_config::load(
        &localpilot_config::ConfigPaths::standard(&cwd),
        &localpilot_config::CliOverrides::default(),
    )?
    .remote;
    if !config.enabled {
        anyhow::bail!(
            "the remote gateway is off. Set `[remote] enabled = true` and `advertised_url` \
             in your user config to turn it on."
        );
    }
    let Some(url) = config.advertised_url.as_deref() else {
        anyhow::bail!(
            "set `[remote] advertised_url` to the HTTPS address your tunnel or proxy \
             forwards to this gateway"
        );
    };
    let origin = crate::server_cmd::pairing_origin(url)?;
    let policy = Policy::new(&origin, &config.proxy_hosts);
    let stop = CancellationToken::new();
    let gateway = Gateway::new(policy, user_discovery()?, stop.clone());
    // Loopback only, always: TLS and any network exposure belong to the
    // user's proxy.
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, config.port)).await?;
    eprintln!(
        "Gateway on http://127.0.0.1:{} for {origin}. It reaches every workspace served with \
         remote access on. Press Ctrl-C to stop.",
        config.port
    );
    tokio::spawn({
        let stop = stop.clone();
        async move {
            let _ = tokio::signal::ctrl_c().await;
            stop.cancel();
        }
    });
    gateway.serve(listener).await;
    eprintln!("Gateway stopped.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn origin_and_host_must_match_exactly_once() {
        let policy = Policy::new("https://pc.example.ts.net", &[]);
        let good = [
            ("host", "pc.example.ts.net"),
            ("origin", "https://pc.example.ts.net"),
        ];
        assert!(policy.origin_ok(&headers(&good)));
        assert!(policy.origin_ok(&headers(&[
            ("host", "PC.Example.ts.net"),
            ("origin", "https://pc.example.ts.net"),
        ])));
        for hostile in [
            // Missing, null or another origin.
            vec![("host", "pc.example.ts.net")],
            vec![("host", "pc.example.ts.net"), ("origin", "null")],
            vec![
                ("host", "pc.example.ts.net"),
                ("origin", "http://pc.example.ts.net"),
            ],
            vec![
                ("host", "pc.example.ts.net"),
                ("origin", "https://pc.example.ts.net.evil.example"),
            ],
            vec![
                ("host", "pc.example.ts.net"),
                ("origin", "https://evil-pc.example.ts.net"),
            ],
            // A rebinding page: right origin is impossible, wrong host.
            vec![
                ("host", "evil.example"),
                ("origin", "https://pc.example.ts.net"),
            ],
            vec![
                ("host", "pc.example.ts.net.evil.example"),
                ("origin", "https://pc.example.ts.net"),
            ],
            vec![
                ("host", "127.0.0.1:47610"),
                ("origin", "https://pc.example.ts.net"),
            ],
            // Duplicates.
            vec![
                ("host", "pc.example.ts.net"),
                ("host", "evil.example"),
                ("origin", "https://pc.example.ts.net"),
            ],
            vec![
                ("host", "pc.example.ts.net"),
                ("origin", "https://pc.example.ts.net"),
                ("origin", "https://evil.example"),
            ],
            // Forwarded headers change nothing.
            vec![
                ("host", "evil.example"),
                ("x-forwarded-host", "pc.example.ts.net"),
                ("origin", "https://pc.example.ts.net"),
            ],
        ] {
            assert!(!policy.origin_ok(&headers(&hostile)), "{hostile:?}");
        }
    }

    #[test]
    fn a_proxy_that_rewrites_host_is_listed_exactly() {
        let policy = Policy::new(
            "https://pc.example.ts.net",
            &["127.0.0.1:47610".to_string()],
        );
        assert!(policy.origin_ok(&headers(&[
            ("host", "127.0.0.1:47610"),
            ("origin", "https://pc.example.ts.net"),
        ])));
        // The listed value replaces the default; nothing else is accepted.
        assert!(!policy.host_ok(&headers(&[("host", "pc.example.ts.net")])));
        assert!(!policy.host_ok(&headers(&[("host", "127.0.0.1:4761")])));
    }

    #[test]
    fn the_device_cookie_is_read_only_when_it_appears_once() {
        let one = headers(&[("cookie", "a=1; __Host-localpilot-device=abc.def; b=2")]);
        assert_eq!(device_cookie(&one).as_deref(), Some("abc.def"));
        let split = headers(&[("cookie", "a=1"), ("cookie", "__Host-localpilot-device=x")]);
        assert_eq!(device_cookie(&split).as_deref(), Some("x"));
        for bad in [
            headers(&[(
                "cookie",
                "__Host-localpilot-device=x; __Host-localpilot-device=y",
            )]),
            headers(&[("cookie", "localpilot-device=x")]),
            headers(&[("cookie", "__Host-localpilot-device=")]),
            headers(&[]),
        ] {
            assert_eq!(device_cookie(&bad), None);
        }
    }

    #[test]
    fn a_frame_becomes_exactly_one_record() {
        let line =
            frame_to_record("{\"v\":1,\"command\":{\"type\":\"prompt\",\"text\":\"a\\nb\"}}")
                .unwrap();
        assert_eq!(line.matches('\n').count(), 1, "{line}");
        assert!(line.ends_with('\n'));
        // A raw newline cannot smuggle a second record: the frame is one
        // JSON value or nothing.
        assert_eq!(
            frame_to_record("{\"v\":1}\n{\"v\":1,\"command\":{\"type\":\"pair_begin\"}}"),
            Err(FrameRefusal::NotJson)
        );
        // Pretty-printed input comes out on one line.
        let pretty = "{\n  \"v\": 1,\n  \"command\": {\"type\": \"status\"}\n}";
        assert_eq!(frame_to_record(pretty).unwrap().matches('\n').count(), 1);
        assert_eq!(frame_to_record("[1]"), Err(FrameRefusal::NotJson));
        assert_eq!(
            frame_to_record(&"x".repeat(MAX_RECORD + 1)),
            Err(FrameRefusal::TooLarge)
        );
    }

    #[test]
    fn records_for_the_gateway_or_the_local_machine_are_refused() {
        for kind in FORBIDDEN_RECORDS {
            let frame = format!("{{\"v\":1,\"command\":{{\"type\":\"{kind}\"}}}}");
            assert_eq!(
                frame_to_record(&frame),
                Err(FrameRefusal::Forbidden),
                "{kind}"
            );
        }
        // A duplicate key resolves to one value, and that value is checked
        // and passed on.
        let duplicate =
            "{\"v\":1,\"command\":{\"type\":\"status\"},\"command\":{\"type\":\"pair_begin\"}}";
        assert_eq!(frame_to_record(duplicate), Err(FrameRefusal::Forbidden));
    }

    /// A relay whose browser side is a 4 KiB pipe nobody reads, and whose
    /// daemon side the test writes records into.
    async fn blocked_relay(
        access: Access,
        stop: CancellationToken,
    ) -> (
        tokio::task::JoinHandle<()>,
        tokio::io::DuplexStream,
        tokio::io::DuplexStream,
    ) {
        let (browser, gateway_side) = tokio::io::duplex(4 * 1024);
        let (daemon_far, daemon_near) = tokio::io::duplex(1024 * 1024);
        let boxed: Box<dyn DaemonStream> = Box::new(daemon_near);
        let (read, write) = tokio::io::split(boxed);
        let daemon = (JsonRecordReader::new(read), write);
        let permit = Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap();
        let first = serde_json::json!({"v": 1, "event": {"type": "closed"}});
        let relay = tokio::spawn(relay(gateway_side, daemon, first, stop, access, permit));
        (relay, browser, daemon_far)
    }

    async fn drain(mut browser: tokio::io::DuplexStream) -> usize {
        use tokio::io::AsyncReadExt;
        let mut received = 0;
        let mut buffer = vec![0_u8; 16 * 1024];
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Ok(n) = browser.read(&mut buffer).await {
                if n == 0 {
                    break;
                }
                received += n;
            }
        })
        .await
        .expect("the socket was closed");
        received
    }

    #[tokio::test]
    async fn a_device_that_loses_access_while_a_record_is_stuck_gets_nothing_more() {
        let allowed = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let access: Access = {
            let allowed = allowed.clone();
            Arc::new(move || allowed.load(std::sync::atomic::Ordering::SeqCst))
        };
        let stop = CancellationToken::new();
        let (relay, browser, mut daemon) = blocked_relay(access, stop.clone()).await;
        // A 64 KiB record the browser is not reading: the write to it blocks.
        let record = serde_json::json!({"v": 1, "event": {"type": "text_delta", "text": "r".repeat(64 * 1024)}});
        let mut line = record.to_string();
        line.push('\n');
        daemon.write_all(line.as_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        // Access ends while the gateway (and the daemon connection) stay up.
        allowed.store(false, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(ACCESS_POLL * 3).await;
        assert!(relay.is_finished(), "the relay stopped on its own");
        assert!(!stop.is_cancelled());
        let received = drain(browser).await;
        assert!(
            received < 64 * 1024,
            "only what was already buffered: {received} bytes"
        );
    }

    #[tokio::test]
    async fn a_device_that_keeps_access_gets_the_whole_record() {
        let access: Access = Arc::new(|| true);
        let (relay, browser, mut daemon) = blocked_relay(access, CancellationToken::new()).await;
        let record = serde_json::json!({"v": 1, "event": {"type": "text_delta", "text": "r".repeat(64 * 1024)}});
        let mut line = record.to_string();
        line.push('\n');
        daemon.write_all(line.as_bytes()).await.unwrap();
        tokio::time::sleep(ACCESS_POLL * 3).await;
        drop(daemon); // the daemon ends the connection after the record
        let received = drain(browser).await;
        assert!(
            received > 64 * 1024,
            "the whole record arrived: {received} bytes"
        );
        relay.await.unwrap();
    }

    fn remote_config(url: Option<&str>) -> localpilot_config::RemoteConfig {
        localpilot_config::RemoteConfig {
            enabled: true,
            advertised_url: url.map(str::to_string),
            port: 9, // discard: nothing answers there
            proxy_hosts: Vec::new(),
        }
    }

    #[tokio::test]
    async fn the_status_shows_state_without_secrets() {
        let home = tempfile::tempdir().unwrap();
        let registry =
            crate::server_devices::DeviceRegistry::open(&home.path().join("devices"), home.path());
        registry
            .add("a", localpilot_rpc::Scope::Control, false)
            .unwrap();
        let (b, _) = registry
            .add("b", localpilot_rpc::Scope::Observe, false)
            .unwrap();
        registry.revoke(&b.id).unwrap();
        let index = crate::server_index::ServedIndex::open(&home.path().join("served"));
        let root = home.path().join("alpha");
        std::fs::create_dir_all(&root).unwrap();
        let _published = index.publish(&root).unwrap();
        let status = status_from(
            Ok(remote_config(Some("https://pc.example.test"))),
            Some(&index),
            &registry,
        )
        .await;
        let text = render_status(&status);
        for expected in [
            "enabled:        yes",
            "advertised url: https://pc.example.test",
            "(127.0.0.1:9)",
            "alpha (",
            "1 paired, 1 revoked",
        ] {
            assert!(text.contains(expected), "{text}");
        }
        assert!(
            !text.contains(&root.to_string_lossy().to_string()),
            "no paths: {text}"
        );
    }

    #[tokio::test]
    async fn the_status_reports_what_it_cannot_read_instead_of_defaults() {
        let home = tempfile::tempdir().unwrap();
        // Unreadable settings: their reason, and no invented port or state.
        let registry =
            crate::server_devices::DeviceRegistry::open(&home.path().join("devices"), home.path());
        let status = status_from(Err("bad TOML at line 3".to_string()), None, &registry).await;
        let text = render_status(&status);
        assert!(
            text.contains("settings:       cannot be read: bad TOML at line 3"),
            "{text}"
        );
        assert!(text.contains("gateway:        unknown"), "{text}");
        assert!(!text.contains("enabled:"), "{text}");

        // An invalid advertised URL is named as invalid, not "not set".
        let status = status_from(
            Ok(remote_config(Some("http://pc.example.test"))),
            None,
            &registry,
        )
        .await;
        let text = render_status(&status);
        assert!(text.contains("advertised url: invalid:"), "{text}");

        // A corrupt registry is reported, not counted as zero devices.
        let devices = home.path().join("corrupt");
        std::fs::create_dir_all(&devices).unwrap();
        std::fs::write(devices.join("registry.lock"), "").unwrap();
        std::fs::write(devices.join("registry.json"), "{ not json").unwrap();
        let corrupt = crate::server_devices::DeviceRegistry::open(&devices, home.path());
        let status = status_from(Ok(remote_config(None)), None, &corrupt).await;
        let text = render_status(&status);
        assert!(text.contains("devices:        cannot be read:"), "{text}");
        assert!(!text.contains("0 paired"), "{text}");
    }

    #[test]
    fn a_websocket_key_must_be_sixteen_bytes_of_canonical_base64() {
        assert!(valid_websocket_key("dGhlIHNhbXBsZSBub25jZQ=="));
        for bad in [
            "abcdefghijklmnopqrstuvwx", // 24 characters, not base64 of 16 bytes
            "dGhlIHNhbXBsZSBub25jZQ=",  // too short
            "dGhlIHNhbXBsZSBub25jZR==", // stray bits in the last symbol
            "dGhlIHNhbXBsZSBub25j!Q==", // not base64
            "dGhlIHNhbXBsZSBub25jZQ",   // no padding
            "dGhlIHNhbXBsZSBub25jZWE=", // 17 bytes
        ] {
            assert!(!valid_websocket_key(bad), "{bad}");
        }
    }

    #[test]
    fn a_socket_names_its_workspace_and_session_explicitly() {
        let w = "0123456789abcdef";
        assert!(matches!(
            socket_query(Some(&format!("workspace={w}&new=1"))),
            Some(SocketQuery {
                target: AttachTarget::OpenNew,
                since: None,
                ..
            })
        ));
        let id = "6f0d7d43-5d6c-4c6c-9b1e-1c2b3d4e5f60";
        assert!(matches!(
            socket_query(Some(&format!("workspace={w}&session={id}&epoch=e1&seq=7"))),
            Some(SocketQuery {
                target: AttachTarget::ResumeId { .. },
                since: Some(FeedCursor { seq: 7, .. }),
                ..
            })
        ));
        for bad in [
            None,
            Some(String::new()),
            Some("new=1".to_string()),
            Some(format!("workspace={w}")),
            Some("workspace=../x&new=1".to_string()),
            Some("workspace=0123456789ABCDEF&new=1".to_string()),
            Some(format!("workspace={w}&workspace=fedcba9876543210&new=1")),
            Some(format!("workspace={w}&new=1&new=1")),
            Some(format!("workspace={w}&session=not-an-id")),
            Some(format!("workspace={w}&new=1&session={id}")),
            Some(format!("workspace={w}&new=1&epoch=e1")),
            Some(format!("workspace={w}&new=1&seq=x&epoch=e")),
            Some(format!("workspace={w}&new=1&other=1")),
        ] {
            assert!(socket_query(bad.as_deref()).is_none(), "{bad:?}");
        }
    }
}
