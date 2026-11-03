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

use crate::remote_status::{
    health_body, render_status, GatewayHealth, RemoteSettings, RemoteStatus,
};

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
const FORBIDDEN_RECORDS: [&str; 12] = [
    "attach",
    "gateway_register",
    "device_attach",
    "device_sessions",
    "pair_begin",
    "pair_hello",
    "pair_open",
    "pair_cancel",
    "pair_status",
    "pair_cancel_receipt",
    "pair_exchange",
    "relay_host",
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
    /// The loopback port the gateway is bound to, which `/identify` must
    /// name in its `Host`.
    bound_port: Option<u16>,
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
            bound_port: None,
        }
    }

    /// The port the gateway listens on; without it `/identify` answers no one.
    pub(crate) fn with_bound_port(mut self, port: u16) -> Self {
        self.bound_port = Some(port);
        self
    }

    /// Exactly one `Host`, and it is `127.0.0.1:<bound port>` or
    /// `[::1]:<bound port>`.
    fn loopback_host(&self, headers: &HeaderMap) -> bool {
        let Some(port) = self.bound_port else {
            return false;
        };
        single(headers, &header::HOST).is_some_and(|host| {
            host == format!("127.0.0.1:{port}") || host == format!("[::1]:{port}")
        })
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

/// A query naming exactly one workspace and nothing else.
fn workspace_query(query: Option<&str>) -> Option<String> {
    let mut workspace = None;
    for pair in query.unwrap_or_default().split('&') {
        match pair.split_once('=')? {
            ("workspace", value) if workspace.is_none() => workspace = Some(value.to_string()),
            _ => return None,
        }
    }
    workspace.filter(|id| crate::server_index::is_discovery_id(id))
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
    limits: Limits,
    /// Pairing attempts, all callers together.
    pairing: std::sync::Mutex<Bucket>,
    /// Requests without a valid device credential, all callers together.
    unauthenticated: std::sync::Mutex<Bucket>,
    rates: std::sync::Mutex<DeviceRates>,
    device_sockets: std::sync::Mutex<DeviceSockets>,
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
        Self::with_settings(policy, discovery, stop, step, Limits::default())
    }

    /// As [`Gateway::with_step`], with its own budgets and timings.
    pub(crate) fn with_settings(
        policy: Policy,
        discovery: Discovery,
        stop: CancellationToken,
        step: Duration,
        limits: Limits,
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
            limits,
            pairing: std::sync::Mutex::new(Bucket::new(limits.pairing)),
            unauthenticated: std::sync::Mutex::new(Bucket::new(limits.unauthenticated)),
            rates: std::sync::Mutex::new(DeviceRates::default()),
            device_sockets: std::sync::Mutex::new(DeviceSockets::default()),
        })
    }

    /// Spend from a global bucket, or the response to send instead.
    fn spend(bucket: &std::sync::Mutex<Bucket>) -> Result<(), Response<Full<Bytes>>> {
        let mut bucket = bucket
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if bucket.take(1.0) {
            Ok(())
        } else {
            Err(too_many(bucket.retry_after()))
        }
    }

    /// The verified device a request's cookie names. A missing or invalid
    /// credential spends only the shared unauthenticated budget.
    fn device(&self, headers: &HeaderMap) -> Result<(String, String), Response<Full<Bytes>>> {
        let verified = device_cookie(headers).and_then(|credential| {
            (self.discovery.verify)(&credential).map(|device| (device, credential))
        });
        match verified {
            Some(found) => Ok(found),
            None => {
                Self::spend(&self.unauthenticated)?;
                Err(plain(StatusCode::UNAUTHORIZED, "this device is not paired"))
            }
        }
    }

    /// Spend from the device's listing or open budget.
    fn spend_device(&self, device: &str, listing: bool) -> Result<(), Response<Full<Bytes>>> {
        self.rates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take(&self.limits, device, listing)
            .map_err(too_many)
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
                gateway.serve_io(stream).await;
                drop(permit);
            });
        }
    }

    /// Serve one client connection. Its transport can be armed with a
    /// device's access while that device's response is written, so an
    /// authenticated HTTP response stops at the transport once access ends.
    pub(crate) async fn serve_io<I>(self: Arc<Self>, io: I)
    where
        I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let armed: Arm = Arc::new(std::sync::Mutex::new(None));
        let io = ArmedIo {
            inner: io,
            armed: armed.clone(),
        };
        let service = hyper::service::service_fn({
            let gateway = self.clone();
            let armed = armed.clone();
            move |request| {
                let gateway = gateway.clone();
                let armed = armed.clone();
                async move { Ok::<_, Infallible>(gateway.route(request, &armed).await) }
            }
        });
        let connection = hyper::server::conn::http1::Builder::new()
            .timer(TokioTimer::new())
            .header_read_timeout(HEADER_TIMEOUT)
            .serve_connection(TokioIo::new(io), service)
            .with_upgrades();
        // Once armed, a connection whose write is stuck is dropped as soon as
        // its access ends, rather than waiting for the client to read again.
        let lost = async {
            let mut poll =
                tokio::time::interval_at(tokio::time::Instant::now() + ACCESS_POLL, ACCESS_POLL);
            loop {
                poll.tick().await;
                let access = armed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                if access.is_some_and(|access| !access()) {
                    return;
                }
            }
        };
        tokio::select! {
            () = self.stop.cancelled() => {}
            () = lost => {}
            _ = connection => {}
        }
    }

    async fn route(
        self: Arc<Self>,
        request: Request<Incoming>,
        armed: &Arm,
    ) -> Response<Full<Bytes>> {
        if !self.policy.host_ok(request.headers()) {
            // Setup identifies the gateway on this computer without knowing
            // the address it was started with. Only the version string is
            // answered, only on this path and only to a loopback-literal
            // `Host`; a page that rebinds its own name to loopback still
            // sends that name. `/healthz` keeps the host policy, so a proxy
            // that rewrites `Host` cannot look healthy there.
            let identify = request.method() == Method::GET && request.uri().path() == IDENTIFY_PATH;
            if identify && self.policy.loopback_host(request.headers()) {
                return plain(StatusCode::OK, &health_body());
            }
            return plain(StatusCode::MISDIRECTED_REQUEST, "unknown host");
        }
        match (request.method(), request.uri().path()) {
            (&Method::GET, "/") => page(),
            (&Method::GET, "/healthz") => plain(StatusCode::OK, &health_body()),
            (&Method::GET, "/api/workspaces") => self.workspaces(&request).await,
            (&Method::GET, "/api/sessions") => self.sessions(&request, armed).await,
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
        let device = match self.device(request.headers()) {
            Ok((device, _)) => device,
            Err(response) => return response,
        };
        if let Err(response) = self.spend_device(&device, true) {
            return response;
        }
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

    /// A workspace's sessions for this device, before it attaches: metadata
    /// only, decided and delivered by the workspace's server.
    async fn sessions(&self, request: &Request<Incoming>, armed: &Arm) -> Response<Full<Bytes>> {
        if !self.policy.origin_ok(request.headers()) {
            return plain(StatusCode::FORBIDDEN, "forbidden");
        }
        let (device, credential) = match self.device(request.headers()) {
            Ok(found) => found,
            Err(response) => return response,
        };
        if let Err(response) = self.spend_device(&device, true) {
            return response;
        }
        let Some(id) = workspace_query(request.uri().query()) else {
            return plain(
                StatusCode::BAD_REQUEST,
                "choose a workspace: ?workspace=<id>",
            );
        };
        let Some(workspace) = self.workspace(&id) else {
            return plain(StatusCode::NOT_FOUND, "that workspace is not being served");
        };
        if !(self.discovery.standing)(&device, &workspace.root) {
            return plain(
                StatusCode::FORBIDDEN,
                "this device may not reach that workspace",
            );
        }
        let Some(live) = self.reach(&workspace).await else {
            return plain(StatusCode::SERVICE_UNAVAILABLE, "that workspace is offline");
        };
        let listed = self
            .ask_daemon(
                &workspace,
                &live,
                ClientCommand::DeviceSessions {
                    capability: live.capability.clone(),
                    credential: WireSecret::new(credential),
                },
            )
            .await;
        match listed {
            Ok((Some(ServerEvent::Sessions { items }), _)) => {
                // Checked again before answering. The size cap bounds what the
                // gateway holds; the armed transport and the connection's
                // access monitor (serve_io) stop and release it once access
                // ends, whether or not the client is reading.
                if live.token.is_cancelled() || !(self.discovery.standing)(&device, &workspace.root)
                {
                    return plain(
                        StatusCode::FORBIDDEN,
                        "this device may not reach that workspace",
                    );
                }
                let body = serde_json::json!({ "sessions": items });
                if body.to_string().len() > crate::server_cmd::DEVICE_SESSIONS_BYTES + 1024 {
                    return plain(StatusCode::BAD_GATEWAY, "the session listing was too large");
                }
                // From here every byte of this connection is written only
                // while the workspace's registration and the device's
                // standing hold; the connection is not reused afterwards.
                let token = live.token.clone();
                let standing = self.discovery.standing.clone();
                let root = workspace.root.clone();
                // The guard is only ever narrowed: a later response on the same
                // connection (pipelining) adds its own check and cannot clear or
                // replace this one.
                let check: Access =
                    Arc::new(move || !token.is_cancelled() && standing(&device, &root));
                let mut slot = armed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *slot = Some(match slot.take() {
                    Some(earlier) => Arc::new(move || earlier() && check()),
                    None => check,
                });
                drop(slot);
                let mut response = json(&body);
                response
                    .headers_mut()
                    .insert(header::CONNECTION, HeaderValue::from_static("close"));
                response
            }
            Ok((Some(ServerEvent::Error { message }), _)) => plain(StatusCode::FORBIDDEN, &message),
            _ => plain(StatusCode::BAD_GATEWAY, "the server did not list sessions"),
        }
    }

    /// Exchange a pairing code (from the QR link's fragment, sent in the body)
    /// for a device credential, which goes only into the device cookie. The
    /// code is presented only to the workspace that issued it.
    async fn pair(&self, request: Request<Incoming>) -> Response<Full<Bytes>> {
        if !self.policy.origin_ok(request.headers()) {
            return plain(StatusCode::FORBIDDEN, "forbidden");
        }
        if let Err(response) = Self::spend(&self.pairing) {
            return response;
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
        // Per-device budgets apply only to a fully verified credential, so a
        // forged secret naming a real device cannot spend that device's budget.
        let (device, credential) = match self.device(headers) {
            Ok(found) => found,
            Err(response) => return response,
        };
        if let Err(response) = self.spend_device(&device, false) {
            return response;
        }
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
        let Some(device_permit) = self
            .device_sockets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .acquire(&device, self.limits.sockets_per_device)
        else {
            return too_many(5);
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
        let limits = self.limits;
        let standing = self.discovery.standing.clone();
        let root = workspace.root.clone();
        let access: Access = Arc::new(move || standing(&device, &root));
        tokio::spawn(async move {
            let upgraded = tokio::select! {
                () = stop.cancelled() => return,
                upgraded = tokio::time::timeout(step, upgraded) => upgraded,
            };
            let Ok(Ok(upgraded)) = upgraded else { return };
            relay(
                TokioIo::new(upgraded),
                daemon,
                first,
                stop,
                access,
                limits,
                (permit, device_permit),
            )
            .await;
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

/// The gateway's budgets and timings. Constants in production; tests shorten
/// them.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    /// How often an open socket is pinged.
    pub(crate) ping_every: Duration,
    /// A socket with no inbound message or control frame for this long is
    /// closed (1001).
    pub(crate) idle_after: Duration,
    /// Inbound messages plus control frames per socket: burst, then per second.
    pub(crate) frames: (f64, f64),
    /// Inbound payload bytes per socket: burst, then per second.
    pub(crate) bytes: (f64, f64),
    /// Browser records waiting for the server: at most this many, and this
    /// many bytes, before the socket is closed (1008).
    pub(crate) queue_records: usize,
    pub(crate) queue_bytes: usize,
    /// Pairing attempts, all callers together: burst, then per second.
    pub(crate) pairing: (f64, f64),
    /// Requests without a valid device credential, all callers together.
    pub(crate) unauthenticated: (f64, f64),
    /// Per verified device: workspace listings and socket opens.
    pub(crate) listings: (f64, f64),
    pub(crate) opens: (f64, f64),
    /// Open sockets per verified device.
    pub(crate) sockets_per_device: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            ping_every: Duration::from_secs(30),
            idle_after: Duration::from_secs(90),
            frames: (50.0, 20.0),
            bytes: (1024.0 * 1024.0, 256.0 * 1024.0),
            queue_records: 16,
            queue_bytes: 1024 * 1024,
            pairing: (10.0, 1.0 / 6.0),
            unauthenticated: (30.0, 1.0),
            listings: (30.0, 0.5),
            opens: (20.0, 1.0 / 3.0),
            sockets_per_device: 8,
        }
    }
}

/// A token bucket on the monotonic clock.
#[derive(Debug, Clone)]
struct Bucket {
    capacity: f64,
    per_second: f64,
    tokens: f64,
    at: std::time::Instant,
}

impl Bucket {
    fn new((capacity, per_second): (f64, f64)) -> Self {
        Self {
            capacity,
            per_second,
            tokens: capacity,
            at: std::time::Instant::now(),
        }
    }

    /// Spend `cost` tokens if there are that many.
    fn take(&mut self, cost: f64) -> bool {
        let now = std::time::Instant::now();
        let refill = now.duration_since(self.at).as_secs_f64() * self.per_second;
        self.tokens = (self.tokens + refill).min(self.capacity);
        self.at = now;
        if self.tokens >= cost {
            self.tokens -= cost;
            true
        } else {
            false
        }
    }

    /// Seconds until one token is available, for `Retry-After`.
    fn retry_after(&self) -> u64 {
        if self.per_second <= 0.0 {
            return 60;
        }
        // At most a minute; never zero.
        ((1.0 - self.tokens).max(0.0) / self.per_second)
            .ceil()
            .clamp(1.0, 60.0) as u64
    }
}

/// Per-device rate buckets, bounded: the least recently used device's
/// buckets are forgotten first (which only refills them; open-socket counts
/// live elsewhere and are never evicted).
#[derive(Default)]
struct DeviceRates {
    devices: HashMap<String, (Bucket, Bucket, std::time::Instant)>,
}

/// At most this many devices' rate buckets are kept.
const MAX_RATE_KEYS: usize = 256;

impl DeviceRates {
    /// Spend from the device's listing (`listing = true`) or open bucket.
    fn take(&mut self, limits: &Limits, device: &str, listing: bool) -> Result<(), u64> {
        if !self.devices.contains_key(device) && self.devices.len() >= MAX_RATE_KEYS {
            if let Some(oldest) = self
                .devices
                .iter()
                .min_by_key(|(_, (_, _, used))| *used)
                .map(|(id, _)| id.clone())
            {
                self.devices.remove(&oldest);
            }
        }
        let entry = self.devices.entry(device.to_string()).or_insert_with(|| {
            (
                Bucket::new(limits.listings),
                Bucket::new(limits.opens),
                std::time::Instant::now(),
            )
        });
        entry.2 = std::time::Instant::now();
        let bucket = if listing { &mut entry.0 } else { &mut entry.1 };
        if bucket.take(1.0) {
            Ok(())
        } else {
            Err(bucket.retry_after())
        }
    }
}

/// Open sockets per device. An entry stays while any permit (or acquisition)
/// holds it; unused entries are reclaimed when others are added, never while
/// live.
#[derive(Default)]
struct DeviceSockets {
    devices: HashMap<String, Arc<Semaphore>>,
}

impl DeviceSockets {
    fn acquire(&mut self, device: &str, per_device: usize) -> Option<OwnedSemaphorePermit> {
        self.devices.retain(|_, slots| {
            Arc::strong_count(slots) > 1 || slots.available_permits() < per_device
        });
        self.devices
            .entry(device.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(per_device)))
            .clone()
            .try_acquire_owned()
            .ok()
    }
}

/// The access a client connection's writes are bound to, once a device's
/// authenticated response is being written on it.
type Arm = Arc<std::sync::Mutex<Option<Access>>>;

/// A client connection's transport: unarmed it passes everything through;
/// armed, every write and flush checks that access still holds.
struct ArmedIo<S> {
    inner: S,
    armed: Arm,
}

impl<S> ArmedIo<S> {
    fn refused(&self) -> Option<std::io::Error> {
        let access = self
            .armed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()?;
        (!access()).then(|| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "the device's access ended",
            )
        })
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for ArmedIo<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for ArmedIo<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if let Some(error) = self.refused() {
            return std::task::Poll::Ready(Err(error));
        }
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if let Some(error) = self.refused() {
            return std::task::Poll::Ready(Err(error));
        }
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// A browser transport whose writes and flushes check the gateway's stop
/// token and the device's standing first, every time. Reads pass through.
struct GuardedIo<S> {
    inner: S,
    stop: CancellationToken,
    access: Access,
}

impl<S> GuardedIo<S> {
    fn refused(&self) -> Option<std::io::Error> {
        (self.stop.is_cancelled() || !(self.access)()).then(|| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "the device's access ended",
            )
        })
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for GuardedIo<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for GuardedIo<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if let Some(error) = self.refused() {
            return std::task::Poll::Ready(Err(error));
        }
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if let Some(error) = self.refused() {
            return std::task::Poll::Ready(Err(error));
        }
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// How a relay ended, and the close code (if any) it is worth telling the
/// browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// Either side ended, the gateway stopped, or access was lost.
    Quiet,
    /// No inbound frame for [`Limits::idle_after`]: 1001.
    Idle,
    /// A budget or the queue bound was exceeded, or a frame was refused: 1008.
    Policy,
}

/// Carry records between an upgraded browser socket and its daemon
/// connection. Three parts run side by side, so none waits on another:
/// sending to the browser (records and pings), reading from the browser
/// (budgets, the idle deadline, and a bounded ordered queue for the server),
/// and writing that queue to the server. Every write is guarded by the stop
/// token and the device's standing. When any part ends the others are
/// dropped; at most one close frame is attempted, for a second, and the
/// transport is dropped regardless.
async fn relay<S: AsyncRead + AsyncWrite + Unpin>(
    socket: S,
    daemon: Daemon,
    first: serde_json::Value,
    stop: CancellationToken,
    access: Access,
    limits: Limits,
    _permits: (OwnedSemaphorePermit, OwnedSemaphorePermit),
) {
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
    use tokio_tungstenite::tungstenite::protocol::frame::CloseFrame;

    let config = WebSocketConfig {
        max_message_size: Some(MAX_RECORD),
        max_frame_size: Some(MAX_RECORD),
        ..WebSocketConfig::default()
    };
    // The halves share one write buffer, and reading can flush it (to answer a
    // ping or a close), so the guard sits on the transport: no path writes to
    // the browser once the gateway stops or the device loses access.
    let socket = GuardedIo {
        inner: socket,
        stop: stop.clone(),
        access: access.clone(),
    };
    let ws = WebSocketStream::from_raw_socket(socket, Role::Server, Some(config)).await;
    let (mut to_browser, mut from_browser) = ws.split();
    let (mut reader, mut to_daemon) = daemon;
    let (queue, mut queued_records) = tokio::sync::mpsc::channel::<String>(limits.queue_records);
    let queued_bytes = std::sync::atomic::AtomicUsize::new(0);

    let outbound = async {
        let mut ping = tokio::time::interval_at(
            tokio::time::Instant::now() + limits.ping_every,
            limits.ping_every,
        );
        let mut next = Some(first);
        loop {
            if let Some(value) = next.take() {
                let text = value.to_string();
                if text.len() > MAX_OUTBOUND {
                    return Ending::Quiet;
                }
                match guarded(&stop, &access, to_browser.send(Message::text(text))).await {
                    Some(Ok(())) => {}
                    _ => return Ending::Quiet,
                }
            }
            // A due ping goes first: a backlog of records that is always
            // ready must not starve the heartbeat the browser answers.
            tokio::select! {
                biased;
                _ = ping.tick() => {
                    match guarded(&stop, &access, to_browser.send(Message::Ping(Vec::new()))).await {
                        Some(Ok(())) => {}
                        _ => return Ending::Quiet,
                    }
                }
                record = reader.next() => match record {
                    Ok(Some(value)) => next = Some(value),
                    _ => return Ending::Quiet, // the daemon ended the device's connection
                },
            }
        }
    };

    let inbound = async {
        let mut frames = Bucket::new(limits.frames);
        let mut bytes = Bucket::new(limits.bytes);
        loop {
            let frame = match tokio::time::timeout(limits.idle_after, from_browser.next()).await {
                Err(_) => return Ending::Idle,
                Ok(Some(Ok(frame))) => frame,
                Ok(_) => return Ending::Quiet,
            };
            // Every delivered message and control frame spends the budget.
            if !frames.take(1.0) || !bytes.take(frame.len() as f64) {
                return Ending::Policy;
            }
            match frame {
                Message::Text(text) => {
                    let Ok(line) = frame_to_record(&text) else {
                        return Ending::Policy;
                    };
                    let size = line.len();
                    let total =
                        queued_bytes.fetch_add(size, std::sync::atomic::Ordering::SeqCst) + size;
                    if total > limits.queue_bytes || queue.try_send(line).is_err() {
                        return Ending::Policy;
                    }
                }
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
                Message::Binary(_) => return Ending::Policy,
                Message::Close(_) => return Ending::Quiet,
            }
        }
    };

    let writer = async {
        while let Some(line) = queued_records.recv().await {
            let write = async {
                to_daemon.write_all(line.as_bytes()).await?;
                to_daemon.flush().await
            };
            match guarded(&stop, &access, write).await {
                Some(Ok(())) => {
                    queued_bytes.fetch_sub(line.len(), std::sync::atomic::Ordering::SeqCst);
                }
                _ => return Ending::Quiet,
            }
        }
        Ending::Quiet
    };

    let ending = tokio::select! {
        () = stop.cancelled() => Ending::Quiet,
        ending = outbound => ending,
        ending = inbound => ending,
        ending = writer => ending,
    };
    let code = match ending {
        Ending::Quiet => return,
        Ending::Idle => CloseCode::Away,
        Ending::Policy => CloseCode::Policy,
    };
    let close = Message::Close(Some(CloseFrame {
        code,
        reason: std::borrow::Cow::Borrowed(""),
    }));
    tokio::select! {
        () = stop.cancelled() => {}
        _ = tokio::time::timeout(Duration::from_secs(1), to_browser.send(close)) => {}
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

/// `429 Too Many Requests`, with when to retry.
fn too_many(retry_after_secs: u64) -> Response<Full<Bytes>> {
    let mut response = plain(
        StatusCode::TOO_MANY_REQUESTS,
        "too many requests; try again later",
    );
    if let Ok(value) = HeaderValue::from_str(&retry_after_secs.to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
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

/// Ask the loopback port for the gateway's health answer. Nothing accepting
/// the connection (refused, or no answer to the connect within the bound, as
/// Windows does for a closed port) is "not running"; a connection that does
/// not give the gateway's answer is something else.
async fn probe(port: u16, host: &str) -> GatewayHealth {
    probe_within(port, host, PROBE_CONNECT, PROBE_EXCHANGE).await
}

/// How long the gateway check may wait to connect, then for its answer. A
/// refused loopback connection can take about two seconds to be reported on
/// Windows, so connecting gets three before the answer is called unknown.
const PROBE_CONNECT: Duration = Duration::from_secs(3);
const PROBE_EXCHANGE: Duration = Duration::from_secs(2);
/// Where the gateway identifies itself to a loopback-literal `Host`.
const IDENTIFY_PATH: &str = "/identify";
/// The most of a reply the gateway checks read; a longer one is not the
/// gateway's.
const PROBE_REPLY_CAP: usize = 64 * 1024;

async fn probe_within(
    port: u16,
    host: &str,
    connect_within: Duration,
    exchange_within: Duration,
) -> GatewayHealth {
    use tokio::io::AsyncReadExt as _;
    let connect = tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port));
    let mut socket = match tokio::time::timeout(connect_within, connect).await {
        Ok(Ok(socket)) => socket,
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
            return GatewayHealth::NotRunning;
        }
        Ok(Err(error)) => return GatewayHealth::CheckFailed(error.to_string()),
        Err(_) => return GatewayHealth::TimedOut,
    };
    let exchange = async {
        socket
            .write_all(
                format!("GET /healthz HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await?;
        let mut raw = Vec::new();
        (&mut socket)
            .take(PROBE_REPLY_CAP as u64 + 1)
            .read_to_end(&mut raw)
            .await?;
        Ok::<_, std::io::Error>(String::from_utf8_lossy(&raw).into_owned())
    };
    match tokio::time::timeout(exchange_within, exchange).await {
        Ok(Ok(text))
            if text.len() <= PROBE_REPLY_CAP
                && text.starts_with("HTTP/1.1 200")
                && text.split_once("\r\n\r\n").map(|(_, body)| body) == Some(&health_body()) =>
        {
            GatewayHealth::Running
        }
        Ok(Ok(text)) if text.is_empty() => GatewayHealth::CheckFailed(
            "the port closed the connection without answering".to_string(),
        ),
        // An answer, but not the gateway's.
        Ok(Ok(_)) => GatewayHealth::Unexpected,
        Ok(Err(error)) => GatewayHealth::CheckFailed(error.to_string()),
        Err(_) => GatewayHealth::TimedOut,
    }
}

/// One raw HTTP/1.1 exchange with whatever listens on the gateway port. It
/// reads until the reply is complete (head plus its declared body), the peer
/// closes, `within` passes or [`PROBE_REPLY_CAP`] is exceeded; a connection
/// kept open after a complete reply does not hold it up.
async fn ask_raw<S>(mut stream: S, request: &[u8], within: Duration) -> Result<Vec<u8>, String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncReadExt as _;
    let exchange = async {
        stream.write_all(request).await?;
        let mut raw = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let read = stream.read(&mut chunk).await?;
            if read == 0 {
                break;
            }
            raw.extend_from_slice(&chunk[..read]);
            if raw.len() > PROBE_REPLY_CAP || status_and_body(&raw).is_some() {
                break;
            }
        }
        Ok::<_, std::io::Error>(raw)
    };
    match tokio::time::timeout(within, exchange).await {
        Ok(Ok(raw)) if raw.len() > PROBE_REPLY_CAP => Err("the answer was too long".to_string()),
        Ok(Ok(raw)) => Ok(raw),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err("no complete answer in time".to_string()),
    }
}

/// The status code and body of a raw reply, when it is a well-formed
/// HTTP/1.1 response whose body is exactly its declared length.
fn status_and_body(raw: &[u8]) -> Option<(u16, &[u8])> {
    let split = raw.windows(4).position(|window| window == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..split]).ok()?;
    let body = &raw[split + 4..];
    let mut lines = head.split("\r\n");
    let status = lines.next()?.strip_prefix("HTTP/1.1 ")?;
    let code: u16 = status.split(' ').next()?.parse().ok()?;
    let length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())?;
    (length == body.len()).then_some((code, body))
}

/// What setup found listening on the gateway port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Listener {
    NotRunning,
    /// The gateway's exact health answer to a loopback-literal `Host`.
    Gateway,
    Unidentified(String),
}

/// How the gateway answered a browser-shaped request for the new address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OriginAnswer {
    /// The exact missing-credential refusal: it accepts this host and origin.
    Current,
    /// The exact refusal of a gateway started with another address.
    OtherPolicy(String),
    Unidentified(String),
}

fn classify_health(raw: &[u8]) -> Listener {
    match status_and_body(raw) {
        Some((200, body)) if body == health_body().as_bytes() => Listener::Gateway,
        Some((code, _)) => Listener::Unidentified(format!("it answered {code}")),
        None => Listener::Unidentified("its answer was not one the gateway gives".to_string()),
    }
}

fn classify_origin(raw: &[u8]) -> OriginAnswer {
    match status_and_body(raw) {
        Some((401, b"this device is not paired")) => OriginAnswer::Current,
        Some((403, b"forbidden")) => {
            OriginAnswer::OtherPolicy("it refuses the new address as an origin".to_string())
        }
        Some((421, b"unknown host")) => {
            OriginAnswer::OtherPolicy("it does not know the new address".to_string())
        }
        Some((code, _)) => OriginAnswer::Unidentified(format!("it answered {code}")),
        None => OriginAnswer::Unidentified("its answer was not one the gateway gives".to_string()),
    }
}

fn health_request(port: u16) -> Vec<u8> {
    format!("GET {IDENTIFY_PATH} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
        .into_bytes()
}

/// A WebSocket upgrade a browser on the new address would send, without a
/// device credential.
fn origin_request(host: &str) -> Vec<u8> {
    origin_request_with_host(host, host)
}

fn origin_request_with_host(host: &str, origin_host: &str) -> Vec<u8> {
    format!(
        "GET /api/socket HTTP/1.1\r\nHost: {host}\r\nOrigin: https://{origin_host}\r\n\
         Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
    )
    .into_bytes()
}

async fn connect_gateway_port(port: u16) -> Result<tokio::net::TcpStream, std::io::Error> {
    let connect = tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port));
    match tokio::time::timeout(PROBE_CONNECT, connect).await {
        Ok(result) => result,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "connecting timed out",
        )),
    }
}

/// Whether the LocalPilot gateway listens on `port`, identified by its exact
/// health answer, whatever address it was started with.
pub(crate) async fn listener_on(port: u16) -> Listener {
    match connect_gateway_port(port).await {
        Ok(stream) => match ask_raw(stream, &health_request(port), PROBE_EXCHANGE).await {
            Ok(raw) => classify_health(&raw),
            Err(why) => Listener::Unidentified(why),
        },
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => Listener::NotRunning,
        Err(error) => Listener::Unidentified(error.to_string()),
    }
}

/// How the gateway on `port` answers a browser on `https://<host>`.
pub(crate) async fn origin_on(port: u16, host: &str) -> OriginAnswer {
    match connect_gateway_port(port).await {
        Ok(stream) => match ask_raw(stream, &origin_request(host), PROBE_EXCHANGE).await {
            Ok(raw) => classify_origin(&raw),
            Err(why) => OriginAnswer::Unidentified(why),
        },
        Err(error) => OriginAnswer::Unidentified(error.to_string()),
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
    let gateway = gateway_of(&settings).await;
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

type Settings = Result<(crate::remote_status::RemoteSettings, Vec<String>), String>;

/// The gateway's health for these settings (`None` when not checked).
async fn gateway_of(settings: &Settings) -> Option<GatewayHealth> {
    match settings {
        // With remote access off there is no gateway to look for.
        Ok((settings, _)) if !settings.enabled => None,
        Ok((settings, proxy_hosts)) => {
            let host = match &settings.advertised {
                Ok(Some(origin)) => Policy::new(origin, proxy_hosts).hosts.first().cloned(),
                _ => None,
            }
            .unwrap_or_else(|| "localhost".to_string());
            Some(probe(settings.port, &host).await)
        }
        Err(_) => None,
    }
}

/// The status for the workspace at `cwd`, with when each part was observed.
pub(crate) async fn observe_at(cwd: &std::path::Path) -> crate::remote_status::ObservedStatus {
    let config = localpilot_config::load(
        &localpilot_config::ConfigPaths::standard(cwd),
        &localpilot_config::CliOverrides::default(),
    )
    .map(|config| config.remote)
    .map_err(|error| error.to_string());
    let settings = settings_of(config);
    // Observed now, before the slower reads that follow.
    let settings_at = std::time::SystemTime::now();
    let index = crate::server_index::ServedIndex::for_user();
    let workspaces = index
        .as_ref()
        .map(crate::server_index::ServedIndex::list)
        .unwrap_or_default()
        .into_iter()
        .map(|entry| (entry.label, entry.id))
        .collect();
    let registry = crate::server_devices::DeviceRegistry::for_user(cwd);
    let checked = settings.clone();
    let gateway = async move { gateway_of(&checked).await };
    observe(settings, settings_at, gateway, workspaces, || {
        device_counts(&registry)
    })
    .await
}

/// Stamp each part with when it was observed: the settings at
/// `settings_at` (when they were read), the gateway when `gateway`
/// resolves, the counts when `counts` is read.
async fn observe(
    settings: Settings,
    settings_at: std::time::SystemTime,
    gateway: impl std::future::Future<Output = Option<GatewayHealth>>,
    workspaces: Vec<(String, String)>,
    counts: impl FnOnce() -> Result<(usize, usize), String>,
) -> crate::remote_status::ObservedStatus {
    let gateway = gateway.await;
    let gateway_at = std::time::SystemTime::now();
    let devices = counts();
    let devices_at = std::time::SystemTime::now();
    crate::remote_status::ObservedStatus {
        status: RemoteStatus {
            settings: settings.map(|(settings, _)| settings),
            gateway,
            workspaces,
            devices,
        },
        settings_at,
        gateway_at,
        devices_at,
    }
}

/// Gather the remote-access status for this user.
pub(crate) async fn status() -> RemoteStatus {
    let cwd = std::env::current_dir().unwrap_or_default();
    status_at(&cwd).await
}

/// [`status`] for the workspace at `cwd`.
pub(crate) async fn status_at(cwd: &std::path::Path) -> RemoteStatus {
    let config = localpilot_config::load(
        &localpilot_config::ConfigPaths::standard(cwd),
        &localpilot_config::CliOverrides::default(),
    )
    .map(|config| config.remote)
    .map_err(|error| error.to_string());
    let index = crate::server_index::ServedIndex::for_user();
    let registry = crate::server_devices::DeviceRegistry::for_user(cwd);
    status_from(config, index.as_ref(), &registry).await
}

/// `localpilot remote status`.
pub(crate) async fn print_status() {
    print!("remote access:\n{}", render_status(&status().await));
}

/// The gateway's listener: loopback only, always. There is no setting that
/// binds anything else.
async fn bind_loopback(port: u16) -> std::io::Result<TcpListener> {
    TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await
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
    let policy = Policy::new(&origin, &config.proxy_hosts).with_bound_port(config.port);
    let stop = CancellationToken::new();
    let gateway = Gateway::new(policy, user_discovery()?, stop.clone());
    // Loopback only, always: TLS and any network exposure belong to the
    // user's proxy.
    let listener = bind_loopback(config.port).await?;
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
        relay_with(access, stop, Limits::default(), 4 * 1024, 1024 * 1024).await
    }

    /// A relay between a browser pipe of `browser_room` bytes and a daemon
    /// pipe of `daemon_room` bytes, with `limits`.
    async fn relay_with(
        access: Access,
        stop: CancellationToken,
        limits: Limits,
        browser_room: usize,
        daemon_room: usize,
    ) -> (
        tokio::task::JoinHandle<()>,
        tokio::io::DuplexStream,
        tokio::io::DuplexStream,
    ) {
        let (browser, gateway_side) = tokio::io::duplex(browser_room);
        let (daemon_far, daemon_near) = tokio::io::duplex(daemon_room);
        let boxed: Box<dyn DaemonStream> = Box::new(daemon_near);
        let (read, write) = tokio::io::split(boxed);
        let daemon = (JsonRecordReader::new(read), write);
        let permit = Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap();
        let device_permit = Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap();
        let first = serde_json::json!({"v": 1, "event": {"type": "closed"}});
        let relay = tokio::spawn(relay(
            gateway_side,
            daemon,
            first,
            stop,
            access,
            limits,
            (permit, device_permit),
        ));
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

    fn quick() -> Limits {
        Limits {
            ping_every: Duration::from_millis(100),
            idle_after: Duration::from_millis(400),
            ..Limits::default()
        }
    }

    /// Short deadlines, but no pings: for tests about the browser's own
    /// frames (a ping would make the test client try to answer it).
    fn quiet() -> Limits {
        Limits {
            ping_every: Duration::from_secs(3600),
            ..quick()
        }
    }

    async fn browser(stream: tokio::io::DuplexStream) -> WebSocketStream<tokio::io::DuplexStream> {
        WebSocketStream::from_raw_socket(stream, Role::Client, None).await
    }

    /// Read until the socket closes; the close code, if one was sent.
    async fn close_code(ws: &mut WebSocketStream<tokio::io::DuplexStream>) -> Option<u16> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match ws.next().await {
                    Some(Ok(Message::Close(frame))) => {
                        return frame.map(|frame| u16::from(frame.code))
                    }
                    Some(Ok(_)) => {}
                    _ => return None,
                }
            }
        })
        .await
        .expect("the socket closed in time")
    }

    #[tokio::test]
    async fn a_flood_of_unsolicited_pongs_closes_the_socket() {
        let (_relay, browser_end, _daemon) = relay_with(
            Arc::new(|| true),
            CancellationToken::new(),
            quick(),
            64 * 1024,
            64 * 1024,
        )
        .await;
        let mut ws = browser(browser_end).await;
        for _ in 0..200 {
            if ws.send(Message::Pong(Vec::new())).await.is_err() {
                break;
            }
        }
        assert_eq!(close_code(&mut ws).await, Some(1008));
    }

    #[tokio::test]
    async fn heartbeat_pongs_keep_a_long_streamed_turn_open() {
        let (relay, browser_end, mut daemon) = relay_with(
            Arc::new(|| true),
            CancellationToken::new(),
            quick(),
            64 * 1024,
            64 * 1024,
        )
        .await;
        let mut ws = browser(browser_end).await;
        // The server streams for well past the idle deadline; the browser
        // sends nothing but its automatic answers to the gateway's pings.
        let streaming = tokio::spawn(async move {
            for n in 0..30 {
                let line =
                    format!("{{\"v\":1,\"event\":{{\"type\":\"text_delta\",\"text\":\"{n}\"}}}}\n");
                daemon.write_all(line.as_bytes()).await.unwrap();
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            daemon
        });
        let mut texts = 0;
        let mut pings = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(3000);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout_at(deadline, ws.next()).await {
                Ok(Some(Ok(Message::Text(_)))) => texts += 1,
                // A browser answers a ping at once.
                Ok(Some(Ok(Message::Ping(payload)))) => {
                    pings += 1;
                    ws.send(Message::Pong(payload)).await.unwrap();
                }
                Ok(Some(Ok(Message::Close(frame)))) => panic!("closed: {frame:?}"),
                Ok(Some(Ok(_))) => {}
                Ok(_) => panic!("the socket ended"),
                Err(_) => break,
            }
        }
        let _daemon = streaming.await.unwrap();
        assert!(texts >= 30, "every record arrived: {texts}");
        assert!(pings >= 3, "pinged throughout: {pings}");
        assert!(
            !relay.is_finished(),
            "still open well past the idle deadline"
        );
    }

    #[tokio::test]
    async fn a_silent_socket_is_closed_as_idle() {
        let (relay, browser_end, _daemon) = relay_with(
            Arc::new(|| true),
            CancellationToken::new(),
            quiet(),
            64 * 1024,
            64 * 1024,
        )
        .await;
        // The browser neither reads nor answers.
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert!(relay.is_finished(), "closed after the idle deadline");
        let mut ws = browser(browser_end).await;
        assert_eq!(close_code(&mut ws).await, Some(1001));
    }

    #[tokio::test]
    async fn a_stalled_server_keeps_the_browser_side_working() {
        // The server never reads: a 1 KiB pipe fills at once.
        let limits = Limits {
            queue_records: 4,
            ..quiet()
        };
        // (a) A silent browser still expires.
        let (relay, browser_end, _daemon) = relay_with(
            Arc::new(|| true),
            CancellationToken::new(),
            limits,
            64 * 1024,
            1024,
        )
        .await;
        let mut ws = browser(browser_end).await;
        let big =
            serde_json::json!({"v": 1, "command": {"type": "prompt", "text": "x".repeat(4096)}});
        ws.send(Message::text(big.to_string())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert!(
            relay.is_finished(),
            "the idle deadline ran while the server write was stuck"
        );
        assert_eq!(close_code(&mut ws).await, Some(1001));

        // (b) Pongs are still processed while the write is stuck.
        let (relay, browser_end, _daemon) = relay_with(
            Arc::new(|| true),
            CancellationToken::new(),
            limits,
            64 * 1024,
            1024,
        )
        .await;
        let mut ws = browser(browser_end).await;
        ws.send(Message::text(big.to_string())).await.unwrap();
        for _ in 0..12 {
            ws.send(Message::Pong(Vec::new())).await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            !relay.is_finished(),
            "answered pongs keep it open past the idle deadline"
        );

        // (c) Input beyond the queue bound closes the socket instead of
        // growing the queue.
        for _ in 0..8 {
            if ws.send(Message::text(big.to_string())).await.is_err() {
                break;
            }
        }
        assert_eq!(close_code(&mut ws).await, Some(1008));
    }

    #[tokio::test]
    async fn a_sustained_backlog_does_not_starve_the_heartbeat() {
        // A backlog that is always ready: the server has already written far
        // more than the gateway can forward in several idle windows.
        let (relay, browser_end, mut daemon) = relay_with(
            Arc::new(|| true),
            CancellationToken::new(),
            quick(),
            64 * 1024,
            16 * 1024 * 1024,
        )
        .await;
        let line = "{\"v\":1,\"event\":{\"type\":\"text_delta\",\"text\":\"x\"}}
";
        daemon
            .write_all(line.repeat(200_000).as_bytes())
            .await
            .unwrap();
        let mut ws = browser(browser_end).await;
        let writing = tokio::spawn(async move { daemon });
        let mut pings = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout_at(deadline, ws.next()).await {
                Ok(Some(Ok(Message::Ping(payload)))) => {
                    pings += 1;
                    ws.send(Message::Pong(payload)).await.unwrap();
                }
                Ok(Some(Ok(Message::Close(frame)))) => panic!("closed: {frame:?}"),
                Ok(Some(Ok(_))) => {}
                Ok(_) => panic!("the socket ended"),
                Err(_) => break,
            }
        }
        let _daemon = writing.await.unwrap();
        assert!(pings >= 3, "pinged during the backlog: {pings}");
        assert!(
            !relay.is_finished(),
            "still open after several idle windows"
        );
    }

    /// Bytes of text payload the browser receives until the socket ends.
    async fn drain_text(ws: &mut WebSocketStream<tokio::io::DuplexStream>) -> usize {
        let mut received = 0;
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(Ok(message)) = ws.next().await {
                if let Message::Text(text) = message {
                    received += text.len();
                }
            }
        })
        .await;
        received
    }

    #[tokio::test]
    async fn a_ping_from_the_browser_cannot_flush_a_revoked_devices_record() {
        let allowed = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let access: Access = {
            let allowed = allowed.clone();
            Arc::new(move || allowed.load(std::sync::atomic::Ordering::SeqCst))
        };
        let (_relay, browser_end, mut daemon) = relay_with(
            access,
            CancellationToken::new(),
            quiet(),
            4 * 1024,
            1024 * 1024,
        )
        .await;
        let mut ws = browser(browser_end).await;
        let record = serde_json::json!({"v": 1, "event": {"type": "text_delta", "text": "r".repeat(64 * 1024)}});
        daemon
            .write_all(format!("{record}\n").as_bytes())
            .await
            .unwrap();
        // The browser has not read: the record is stuck in the gateway's
        // buffer. Revoke, then at once ask for a pong and drain.
        tokio::time::sleep(Duration::from_millis(100)).await;
        allowed.store(false, std::sync::atomic::Ordering::SeqCst);
        ws.send(Message::Ping(Vec::new())).await.unwrap();
        let received = drain_text(&mut ws).await;
        assert!(
            received < 64 * 1024,
            "the stuck record was not flushed: {received}"
        );
    }

    #[tokio::test]
    async fn closing_an_idle_revoked_socket_cannot_flush_its_record() {
        let allowed = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let access: Access = {
            let allowed = allowed.clone();
            Arc::new(move || allowed.load(std::sync::atomic::Ordering::SeqCst))
        };
        // The idle deadline (100 ms) comes before the access poll (250 ms),
        // so the relay ends as idle and tries to send a close.
        let limits = Limits {
            idle_after: Duration::from_millis(100),
            ..quiet()
        };
        let (relay, browser_end, mut daemon) = relay_with(
            access,
            CancellationToken::new(),
            limits,
            4 * 1024,
            1024 * 1024,
        )
        .await;
        let record = serde_json::json!({"v": 1, "event": {"type": "text_delta", "text": "r".repeat(64 * 1024)}});
        daemon
            .write_all(format!("{record}\n").as_bytes())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        allowed.store(false, std::sync::atomic::Ordering::SeqCst);
        let mut ws = browser(browser_end).await;
        let received = drain_text(&mut ws).await;
        assert!(relay.is_finished());
        assert!(
            received < 64 * 1024,
            "the close did not flush the record: {received}"
        );
    }

    #[tokio::test]
    async fn the_gateway_listens_on_loopback_only() {
        let listener = bind_loopback(0).await.unwrap();
        let address = listener.local_addr().unwrap();
        assert!(address.ip().is_loopback(), "{address}");
        // This machine's own non-loopback address, found without sending
        // anything (a connected UDP socket picks the outgoing interface).
        let outward = std::net::UdpSocket::bind("0.0.0.0:0")
            .and_then(|socket| socket.connect("192.0.2.1:9").map(|()| socket))
            .and_then(|socket| socket.local_addr())
            .map(|local| local.ip())
            .ok()
            .filter(|ip| !ip.is_loopback() && !ip.is_unspecified());
        let Some(ip) = outward else {
            eprintln!("SKIPPED: this machine has no non-loopback address to test against");
            return;
        };
        let attempt = tokio::time::timeout(
            Duration::from_secs(3),
            tokio::net::TcpStream::connect((ip, address.port())),
        )
        .await;
        assert!(
            !matches!(attempt, Ok(Ok(_))),
            "a connection to {ip}:{} must be refused",
            address.port()
        );
    }

    /// A fake server for one workspace: it registers the gateway (naming
    /// `id`) and answers `device_sessions` with `items`.
    fn fake_server(
        id: &str,
        items: Vec<localpilot_rpc::SessionInfo>,
        control_ends: CancellationToken,
    ) -> Dial {
        let id = id.to_string();
        Arc::new(move |_workspace: &Workspace| {
            let (near, far) = tokio::io::duplex(1024 * 1024);
            let id = id.clone();
            let items = items.clone();
            let control_ends = control_ends.clone();
            tokio::spawn(async move {
                let (read, mut write) = tokio::io::split(far);
                let mut reader = JsonRecordReader::new(read);
                let Ok(Some(first)) = reader.next().await else {
                    return;
                };
                let kind = first["command"]["type"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let event = match kind.as_str() {
                    "gateway_register" => ServerEvent::GatewayRegistered {
                        capability: WireSecret::new("cap"),
                        workspace: Some(id),
                    },
                    _ => ServerEvent::Sessions { items },
                };
                let record = ServerRecord {
                    v: RPC_PROTOCOL_VERSION,
                    id: None,
                    seq: None,
                    event,
                };
                let mut line = serde_json::to_vec(&record).unwrap();
                line.push(b'\n');
                let _ = write.write_all(&line).await;
                if kind == "gateway_register" {
                    // The control channel lives until the test ends it.
                    tokio::select! {
                        () = control_ends.cancelled() => {}
                        _ = async { while let Ok(Some(_)) = reader.next().await {} } => {}
                    }
                }
            });
            Box::pin(async move { Ok(Box::new(near) as Box<dyn DaemonStream>) })
        })
    }

    /// How access ends in [`listing_after`].
    #[derive(Clone, Copy, PartialEq)]
    enum Loss {
        None,
        Revoked,
        ControlEnded,
    }

    /// Serve one `/api/sessions` request over a 4 KiB pipe; once the
    /// response has stalled, end access as `loss` says. Returns whether the
    /// connection ended by itself (nobody reading), then the bytes drained
    /// and the full listing's length.
    async fn listing_after(loss: Loss) -> (bool, usize, usize) {
        let id = "0123456789abcdef".to_string();
        let items: Vec<localpilot_rpc::SessionInfo> = (0..28)
            .map(|n| localpilot_rpc::SessionInfo {
                id: localpilot_core::SessionId::new(),
                name: Some(format!("{n}-{}", "s".repeat(2000))),
                live: false,
                message_count: 1,
                updated_unix: n,
            })
            .collect();
        let full = serde_json::json!({ "sessions": items }).to_string().len();
        let allowed = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let control_ends = CancellationToken::new();
        let discovery = Discovery {
            list: {
                let id = id.clone();
                Arc::new(move || {
                    vec![Workspace {
                        id: id.clone(),
                        label: "w".to_string(),
                        root: std::env::temp_dir(),
                    }]
                })
            },
            dial: fake_server(&id, items, control_ends.clone()),
            verify: Arc::new(|_| Some("device".to_string())),
            standing: {
                let allowed = allowed.clone();
                Arc::new(move |_, _| allowed.load(std::sync::atomic::Ordering::SeqCst))
            },
        };
        let gateway = Gateway::new(
            Policy::new("https://pc.example.test", &[]),
            discovery,
            CancellationToken::new(),
        );
        let (mut client, server) = tokio::io::duplex(4 * 1024);
        let connection = tokio::spawn(gateway.serve_io(server));
        let request = format!(
            "GET /api/sessions?workspace={id} HTTP/1.1\r\nHost: pc.example.test\r\n\
             Origin: https://pc.example.test\r\nCookie: {DEVICE_COOKIE}=x.y.z\r\n\r\n"
        );
        client.write_all(request.as_bytes()).await.unwrap();
        // The response fills the pipe and stalls; then access changes.
        tokio::time::sleep(Duration::from_millis(300)).await;
        match loss {
            Loss::None => {}
            Loss::Revoked => allowed.store(false, std::sync::atomic::Ordering::SeqCst),
            Loss::ControlEnded => control_ends.cancel(),
        }
        // Without reading anything, wait past the access poll.
        tokio::time::sleep(ACCESS_POLL * 3).await;
        let ended_by_itself = connection.is_finished();
        let mut received = Vec::new();
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::io::AsyncReadExt::read_to_end(&mut client, &mut received),
        )
        .await;
        (ended_by_itself, received.len(), full)
    }

    #[tokio::test]
    async fn a_stalled_session_listing_stops_at_the_transport_when_access_ends() {
        for loss in [Loss::Revoked, Loss::ControlEnded] {
            let (ended, received, full) = listing_after(loss).await;
            assert!(ended, "the stuck connection was released without any read");
            assert!(
                received < full,
                "the rest was not sent: {received} of {full}"
            );
        }
        let (ended, received, full) = listing_after(Loss::None).await;
        assert!(
            !ended,
            "an authorized stalled connection waits for its reader"
        );
        assert!(
            received > full,
            "authorized: the whole response arrived: {received} of {full}"
        );
    }

    #[test]
    fn a_devices_socket_count_survives_rate_eviction() {
        let limits = Limits::default();
        let mut sockets = DeviceSockets::default();
        let held: Vec<_> = (0..limits.sockets_per_device)
            .map(|_| sockets.acquire("busy", limits.sockets_per_device).unwrap())
            .collect();
        assert!(sockets.acquire("busy", limits.sockets_per_device).is_none());
        // Many other devices come and go; the busy one's count is kept.
        let mut rates = DeviceRates::default();
        for n in 0..(MAX_RATE_KEYS * 2) {
            let _ = rates.take(&limits, &format!("d{n}"), true);
            let other = sockets.acquire(&format!("d{n}"), limits.sockets_per_device);
            drop(other);
        }
        assert!(sockets.acquire("busy", limits.sockets_per_device).is_none());
        assert!(
            sockets.devices.len() <= 2,
            "unused entries are reclaimed: {}",
            sockets.devices.len()
        );
        drop(held);
        assert!(sockets.acquire("busy", limits.sockets_per_device).is_some());
        assert!(rates.devices.len() <= MAX_RATE_KEYS);
    }

    #[test]
    fn a_bucket_refills_over_time_and_says_when_to_retry() {
        let mut bucket = Bucket::new((2.0, 10.0));
        assert!(bucket.take(1.0) && bucket.take(1.0));
        assert!(!bucket.take(1.0));
        assert!((1..=60).contains(&bucket.retry_after()));
        std::thread::sleep(Duration::from_millis(150));
        assert!(bucket.take(1.0));
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

    fn plain_gateway(origin: &str, hosts: &[String]) -> Arc<Gateway> {
        let discovery = Discovery {
            list: Arc::new(Vec::new),
            dial: Arc::new(|_: &Workspace| {
                Box::pin(async {
                    Err::<Box<dyn DaemonStream>, _>(std::io::Error::other("no daemon"))
                })
            }),
            verify: Arc::new(|_| None),
            standing: Arc::new(|_, _| false),
        };
        Gateway::new(
            Policy::new(origin, hosts).with_bound_port(47610),
            discovery,
            CancellationToken::new(),
        )
    }

    async fn ask_gateway(gateway: &Arc<Gateway>, request: Vec<u8>) -> Vec<u8> {
        let (client, server) = tokio::io::duplex(64 * 1024);
        tokio::spawn(gateway.clone().serve_io(server));
        ask_raw(client, &request, Duration::from_secs(5))
            .await
            .expect("an answer")
    }

    #[tokio::test]
    async fn setup_identifies_the_gateway_and_its_address_exactly() {
        let current = plain_gateway("https://pc.tail1.ts.net", &[]);
        // The identify path answers a loopback-literal Host whatever the
        // gateway's address, and nothing else is opened to it.
        assert_eq!(
            classify_health(&ask_gateway(&current, health_request(47610)).await),
            Listener::Gateway
        );
        let other_path = b"GET / HTTP/1.1\r\nHost: 127.0.0.1:47610\r\nConnection: close\r\n\r\n";
        let raw = ask_gateway(&current, other_path.to_vec()).await;
        assert_eq!(status_and_body(&raw).map(|(code, _)| code), Some(421));
        let rebound =
            b"GET /identify HTTP/1.1\r\nHost: evil.example:47610\r\nConnection: close\r\n\r\n";
        let raw = ask_gateway(&current, rebound.to_vec()).await;
        assert_eq!(status_and_body(&raw).map(|(code, _)| code), Some(421));
        // `/healthz` keeps the host policy for loopback too.
        let rebound =
            b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1:47610\r\nConnection: close\r\n\r\n";
        let raw = ask_gateway(&current, rebound.to_vec()).await;
        assert_eq!(status_and_body(&raw).map(|(code, _)| code), Some(421));

        // A loopback Host opens nothing else: pairing, sessions, the socket.
        for request in [
            "POST /api/pair HTTP/1.1\r\nHost: 127.0.0.1:47610\r\nOrigin: https://pc.tail1.ts.net\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "GET /api/sessions?workspace=x HTTP/1.1\r\nHost: 127.0.0.1:47610\r\nConnection: close\r\n\r\n",
            "GET /api/workspaces HTTP/1.1\r\nHost: 127.0.0.1:47610\r\nConnection: close\r\n\r\n",
        ] {
            let raw = ask_gateway(&current, request.as_bytes().to_vec()).await;
            assert_eq!(status_and_body(&raw).map(|(code, _)| code), Some(421), "{request}");
        }
        let raw = ask_gateway(
            &current,
            origin_request_with_host("127.0.0.1:47610", "pc.tail1.ts.net"),
        )
        .await;
        assert_eq!(status_and_body(&raw).map(|(code, _)| code), Some(421));
        // Another port, or no port, is not this gateway's loopback name.
        let raw = ask_gateway(
            &current,
            b"GET /identify HTTP/1.1\r\nHost: 127.0.0.1:1\r\nConnection: close\r\n\r\n".to_vec(),
        )
        .await;
        assert_eq!(status_and_body(&raw).map(|(code, _)| code), Some(421));

        // The new address: accepted, so refused only for the missing credential.
        assert_eq!(
            classify_origin(&ask_gateway(&current, origin_request("pc.tail1.ts.net")).await),
            OriginAnswer::Current
        );
        // A gateway started with an older address does not know the host.
        let older = plain_gateway("https://old.tail1.ts.net", &[]);
        assert!(matches!(
            classify_origin(&ask_gateway(&older, origin_request("pc.tail1.ts.net")).await),
            OriginAnswer::OtherPolicy(_)
        ));
        // A proxy host lets the request in, but the origin is still the old one.
        let proxied = plain_gateway("https://old.tail1.ts.net", &["pc.tail1.ts.net".to_string()]);
        assert_eq!(
            classify_health(&ask_gateway(&proxied, health_request(47610)).await),
            Listener::Gateway
        );
        assert!(matches!(
            classify_origin(&ask_gateway(&proxied, origin_request("pc.tail1.ts.net")).await),
            OriginAnswer::OtherPolicy(_)
        ));
    }

    #[test]
    fn answers_that_are_not_the_gateways_are_unidentified() {
        let reply = |code: u16, body: &str| {
            format!(
                "HTTP/1.1 {code} X\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_bytes()
        };
        assert!(matches!(
            classify_health(&reply(200, "hello")),
            Listener::Unidentified(_)
        ));
        assert!(matches!(
            classify_health(&reply(421, "unknown host")),
            Listener::Unidentified(_)
        ));
        for raw in [
            reply(429, "slow down"),
            reply(401, "this device is not paired!"),
            reply(101, ""),
            // A body longer or shorter than declared.
            b"HTTP/1.1 401 X\r\nContent-Length: 99\r\n\r\nthis device is not paired".to_vec(),
            // No length at all, or not HTTP/1.1.
            b"HTTP/1.1 401 X\r\n\r\nthis device is not paired".to_vec(),
            b"HTTP/1.0 401 X\r\nContent-Length: 25\r\n\r\nthis device is not paired".to_vec(),
            Vec::new(),
        ] {
            assert!(
                matches!(classify_origin(&raw), OriginAnswer::Unidentified(_)),
                "{}",
                String::from_utf8_lossy(&raw)
            );
        }
        let policy = Policy::new("https://pc.tail1.ts.net", &[]).with_bound_port(47610);
        assert!(policy.loopback_host(&headers(&[("host", "127.0.0.1:47610")])));
        assert!(policy.loopback_host(&headers(&[("host", "[::1]:47610")])));
        for host in [
            "127.0.0.1",
            "127.0.0.1:47611",
            "127.0.0.1:047610",
            "[::1]",
            "localhost:47610",
            "127.0.0.2:47610",
            "127.0.0.1.evil:47610",
            "pc",
        ] {
            assert!(!policy.loopback_host(&headers(&[("host", host)])), "{host}");
        }
        // Twice, or without a bound port: never.
        assert!(!policy.loopback_host(&headers(&[
            ("host", "127.0.0.1:47610"),
            ("host", "127.0.0.1:47610")
        ])));
        let unbound = Policy::new("https://pc.tail1.ts.net", &[]);
        assert!(!unbound.loopback_host(&headers(&[("host", "127.0.0.1:47610")])));
    }

    #[tokio::test]
    async fn an_oversized_answer_is_never_the_gateway() {
        let (mut client, mut server) = tokio::io::duplex(256 * 1024);
        tokio::spawn(async move {
            let mut sink = [0u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut server, &mut sink).await;
            let mut body = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                PROBE_REPLY_CAP + 10
            )
            .into_bytes();
            body.extend(std::iter::repeat(b'x').take(PROBE_REPLY_CAP + 10));
            let _ = server.write_all(&body).await;
        });
        let _ = &mut client;
        assert!(ask_raw(client, &health_request(1), Duration::from_secs(5))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn the_gateway_check_says_only_what_it_found() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // Nothing listens: refused.
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = free.local_addr().unwrap().port();
        drop(free);
        assert_eq!(
            probe_within(port, "h", PROBE_CONNECT, Duration::from_secs(1)).await,
            GatewayHealth::NotRunning
        );
        let serve = |reply: Option<&'static str>, stall: bool| async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 256];
                let _ = socket.read(&mut request).await;
                if stall {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                if let Some(reply) = reply {
                    let _ = socket.write_all(reply.as_bytes()).await;
                }
            });
            port
        };
        // Accepts and never answers: unknown (timed out), not "something else".
        let port = serve(None, true).await;
        assert_eq!(
            probe_within(
                port,
                "h",
                Duration::from_secs(1),
                Duration::from_millis(300)
            )
            .await,
            GatewayHealth::TimedOut
        );
        // Closes without a word: the check failed, not proof of anything.
        let port = serve(None, false).await;
        assert!(matches!(
            probe_within(port, "h", Duration::from_secs(1), Duration::from_secs(1)).await,
            GatewayHealth::CheckFailed(_)
        ));
        // A valid HTTP answer that is not the gateway's: something else.
        let port = serve(
            Some("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nhello"),
            false,
        )
        .await;
        assert_eq!(
            probe_within(port, "h", Duration::from_secs(1), Duration::from_secs(1)).await,
            GatewayHealth::Unexpected
        );
        // The gateway's own answer.
        let body = format!(
            "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{}",
            health_body()
        );
        let port = serve(Some(Box::leak(body.into_boxed_str())), false).await;
        assert_eq!(
            probe_within(port, "h", Duration::from_secs(1), Duration::from_secs(1)).await,
            GatewayHealth::Running
        );
    }
    #[tokio::test]
    async fn each_part_of_a_status_is_stamped_when_it_was_observed() {
        let settings: Settings = Ok((
            crate::remote_status::RemoteSettings {
                enabled: true,
                advertised: Ok(None),
                port: 1,
            },
            Vec::new(),
        ));
        let slow_gateway = async {
            tokio::time::sleep(std::time::Duration::from_millis(60)).await;
            Some(GatewayHealth::Running)
        };
        let observed = observe(
            settings,
            std::time::SystemTime::now(),
            slow_gateway,
            Vec::new(),
            || {
                std::thread::sleep(std::time::Duration::from_millis(30));
                Ok((1, 0))
            },
        )
        .await;
        let gap = |a: std::time::SystemTime, b: std::time::SystemTime| {
            b.duration_since(a).unwrap_or_default()
        };
        assert!(
            gap(observed.settings_at, observed.gateway_at) >= std::time::Duration::from_millis(50)
        );
        assert!(
            gap(observed.gateway_at, observed.devices_at) >= std::time::Duration::from_millis(20)
        );
        assert_eq!(observed.status.gateway, Some(GatewayHealth::Running));
    }
}
