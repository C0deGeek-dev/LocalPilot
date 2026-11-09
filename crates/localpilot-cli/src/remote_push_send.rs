//! Phone notifications: when a session this server relays waits on a
//! permission ask or stops, send each phone that turned notifications on a
//! content-free, end-to-end encrypted Web Push through its browser's push
//! service.
//!
//! What triggers a push is only what the relay mirror committed: a verified
//! snapshot (asks already open, never a past stop) and in-order events. Each
//! ask and each stop is notified at most once per server run. A phone gets at
//! most one push of a kind per window, with a trailing re-check so an ask
//! still open at the window's end is not lost. Every send is re-checked just
//! before it goes (the device's standing, the exact subscription revision,
//! the current key, the session still relayed here) and goes only to a
//! pinned public address of an allowed push service, with no proxy and no
//! redirect, under one deadline that starts before the name is resolved.
//!
//! Stopping (revoke, share stop, notifications off) prevents new sends only:
//! a push service may still deliver one it already accepted, later.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::future::BoxFuture;
use localpilot_core::SessionId;

use crate::remote_push_endpoint::{check_endpoint, check_keys, is_public};
use crate::remote_push_keys::PushKeys;
use crate::server_devices::{DeviceRegistry, PushSubscription};

/// One push of a kind per device per this window (per workspace server).
pub(crate) const WINDOW: Duration = Duration::from_secs(30);
/// The whole send: resolve, connect, TLS, request and the answer.
const SEND_DEADLINE: Duration = Duration::from_secs(10);
/// How much of a push service's answer is read.
const ANSWER_CAP: usize = 4096;
/// Ask ids remembered per session as already notified.
const NOTIFIED_CAP: usize = 256;
/// Every notification's padded plaintext size.
const PADDED: usize = 32;
/// The longest an ask notification is kept by the push service.
const ASK_TTL_SECS: u64 = 300;
const STOPPED_TTL_SECS: u64 = 900;

/// What a notification says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Kind {
    Ask,
    Stopped,
    /// A test the device asked for.
    Test,
}

impl Kind {
    fn payload(self) -> &'static [u8] {
        match self {
            Self::Ask => br#"{"k":"ask"}"#,
            Self::Stopped => br#"{"k":"stopped"}"#,
            Self::Test => br#"{"k":"test"}"#,
        }
    }

    fn urgency(self) -> &'static str {
        match self {
            Self::Ask => "high",
            Self::Stopped | Self::Test => "normal",
        }
    }
}

/// One request to a push service, fully prepared.
pub(crate) struct Outgoing {
    pub(crate) endpoint: String,
    pub(crate) host: String,
    pub(crate) ttl_secs: u64,
    pub(crate) urgency: &'static str,
    pub(crate) authorization: String,
    pub(crate) body: Vec<u8>,
}

/// Why a send did not get an answer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum SendError {
    #[error("the push service's address is not public")]
    NotPublic,
    #[error("the push service's name did not resolve")]
    Unresolved,
    #[error("the push service did not answer in time")]
    TimedOut,
    #[error("the request failed")]
    Failed,
    /// Not sent: what it was for no longer held after the name resolved.
    #[error("no longer due")]
    Withdrawn,
}

/// Resolve a host to addresses (a seam for tests).
pub(crate) type Resolve =
    Arc<dyn Fn(String) -> BoxFuture<'static, std::io::Result<Vec<SocketAddr>>> + Send + Sync>;
/// Send one prepared request to pinned addresses (a seam for tests).
pub(crate) type Post = Arc<
    dyn Fn(
            Outgoing,
            Vec<SocketAddr>,
            tokio::time::Instant,
        ) -> BoxFuture<'static, Result<u16, SendError>>
        + Send
        + Sync,
>;

/// The system resolver.
pub(crate) fn system_resolve() -> Resolve {
    Arc::new(|host: String| {
        Box::pin(async move {
            Ok(tokio::net::lookup_host((host.as_str(), 443))
                .await?
                .collect())
        })
    })
}

/// The real request: HTTPS only, no proxy, no redirect, the name pinned to
/// the checked addresses (so it is never resolved again), and the answer
/// read only up to a small cap.
pub(crate) fn https_post() -> Post {
    Arc::new(
        |out: Outgoing, addresses: Vec<SocketAddr>, deadline: tokio::time::Instant| {
            Box::pin(async move {
                let left = deadline.saturating_duration_since(tokio::time::Instant::now());
                let client = reqwest::Client::builder()
                    .https_only(true)
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .resolve_to_addrs(&out.host, &addresses)
                    .timeout(left)
                    .build()
                    .map_err(|_| SendError::Failed)?;
                let response = client
                    .post(&out.endpoint)
                    .header("TTL", out.ttl_secs.to_string())
                    .header("Urgency", out.urgency)
                    .header("Content-Encoding", "aes128gcm")
                    .header("Content-Type", "application/octet-stream")
                    .header("Authorization", &out.authorization)
                    .body(out.body)
                    .send()
                    .await
                    .map_err(|error| {
                        if error.is_timeout() {
                            SendError::TimedOut
                        } else {
                            SendError::Failed
                        }
                    })?;
                let status = response.status().as_u16();
                let mut response = response;
                let mut read = 0usize;
                while read < ANSWER_CAP {
                    match response.chunk().await {
                        Ok(Some(chunk)) => read += chunk.len(),
                        _ => break,
                    }
                }
                Ok(status)
            })
        },
    )
}

/// Resolve and check the push service's address, then send, all under one
/// deadline that starts before the name is resolved.
pub(crate) async fn deliver(
    resolve: &Resolve,
    post: &Post,
    out: Outgoing,
    recheck: &(dyn Fn() -> bool + Sync),
) -> Result<u16, SendError> {
    let deadline = tokio::time::Instant::now() + SEND_DEADLINE;
    tokio::time::timeout_at(deadline, async {
        let addresses = resolve(out.host.clone())
            .await
            .map_err(|_| SendError::Unresolved)?;
        if addresses.is_empty() {
            return Err(SendError::Unresolved);
        }
        // Every address must be public: one private answer refuses the
        // whole name (a rebinding name cannot pick its target).
        if !addresses.iter().all(|address| is_public(address.ip())) {
            return Err(SendError::NotPublic);
        }
        if !recheck() {
            return Err(SendError::Withdrawn);
        }
        post(out, addresses, deadline).await
    })
    .await
    .unwrap_or(Err(SendError::TimedOut))
}

/// What the relay mirror tells the sender.
#[derive(Debug, Clone)]
pub(crate) enum Mirror {
    /// A verified snapshot: the asks open now. The session is eligible again.
    Baseline(Vec<(String, u64)>),
    /// An ask opened (id, expiry in ms since the epoch; 0 = none).
    AskOpened(String, u64),
    /// An ask ended.
    AskEnded(String),
    /// The session's turn stopped, at feed position (epoch, seq).
    Stopped(String, u64),
    /// The mirror lost track (a gap, or a snapshot being rebuilt): nothing is
    /// sent for this session until the next verified baseline.
    Gap,
    /// The session is no longer relayed here.
    Gone,
}

#[derive(Default)]
struct SessionState {
    open: BTreeMap<String, u64>,
    notified: VecDeque<String>,
    notified_set: HashSet<String>,
    stops: HashSet<(String, u64)>,
    stop_pending: bool,
    /// Bumped whenever what the mirror knows is lost (a gap, a rebuild, the
    /// relay ending): work queued for an older generation is never sent.
    generation: u64,
    /// A verified baseline holds for `generation`.
    live: bool,
}

impl SessionState {
    fn remember(&mut self, ask: &str) -> bool {
        if self.notified_set.contains(ask) {
            return false;
        }
        self.notified.push_back(ask.to_string());
        self.notified_set.insert(ask.to_string());
        while self.notified.len() > NOTIFIED_CAP {
            if let Some(old) = self.notified.pop_front() {
                self.notified_set.remove(&old);
            }
        }
        true
    }

    fn lose_track(&mut self) {
        self.generation += 1;
        self.live = false;
        self.open.clear();
        self.stop_pending = false;
    }
}

/// What a push is for: one session, as the mirror knew it then.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Cause {
    session: SessionId,
    generation: u64,
}

#[derive(Default)]
struct Window {
    last_sent: Option<tokio::time::Instant>,
    /// What arrived inside the window, for the trailing re-check.
    pending: Vec<Cause>,
    trailing: bool,
}

#[derive(Default)]
struct State {
    sessions: HashMap<SessionId, SessionState>,
    windows: HashMap<(String, Kind), Window>,
    /// When each device last asked for a test notification.
    tests: HashMap<String, tokio::time::Instant>,
}

/// How often a device may ask for a test notification.
const TEST_EVERY: Duration = Duration::from_secs(30);
/// How long the push service keeps a test notification.
const TEST_TTL_SECS: u64 = 60;

/// What became of a device's test notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TestOutcome {
    /// The push service accepted it.
    Sent,
    /// The device has no saved subscription, or it was made for a key that
    /// is no longer current.
    NoSubscription,
    /// Asked again too soon: wait this many seconds.
    TooSoon(u64),
    /// The push service says the subscription no longer exists; it was
    /// removed.
    Gone,
    /// The device may no longer be notified.
    NotAllowed,
    /// The push service could not be reached or refused it.
    Failed,
}

impl TestOutcome {
    /// The wire name of the outcome.
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::NoSubscription => "no_subscription",
            Self::TooSoon(_) => "too_soon",
            Self::Gone => "gone",
            Self::NotAllowed => "not_allowed",
            Self::Failed => "failed",
        }
    }
}

/// The sender for one workspace server.
pub(crate) struct PushSender {
    registry: Arc<DeviceRegistry>,
    keys: Arc<PushKeys>,
    resolve: Resolve,
    post: Post,
    state: Mutex<State>,
    window: Duration,
}

impl PushSender {
    pub(crate) fn new(registry: Arc<DeviceRegistry>, keys: Arc<PushKeys>) -> Arc<Self> {
        Self::with(registry, keys, system_resolve(), https_post(), WINDOW)
    }

    pub(crate) fn with(
        registry: Arc<DeviceRegistry>,
        keys: Arc<PushKeys>,
        resolve: Resolve,
        post: Post,
        window: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            registry,
            keys,
            resolve,
            post,
            state: Mutex::new(State::default()),
            window,
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Take one committed mirror fact for `session`.
    pub(crate) fn observe(self: &Arc<Self>, session: SessionId, fact: Mirror) {
        let wanted = {
            let mut state = self.state();
            let entry = state.sessions.entry(session).or_default();
            let cause = Cause {
                session,
                generation: entry.generation,
            };
            let mut kinds = Vec::new();
            match fact {
                Mirror::Baseline(asks) => {
                    entry.live = true;
                    entry.open = asks.iter().cloned().collect();
                    // An ask already open gets one notification; one notified
                    // before (a rebuild or reconnect) gets none.
                    if asks
                        .iter()
                        .fold(false, |any, (id, _)| entry.remember(id) || any)
                    {
                        kinds.push(Kind::Ask);
                    }
                }
                // Events count only on top of a verified baseline.
                _ if !entry.live && !matches!(fact, Mirror::Gap | Mirror::Gone) => {}
                Mirror::AskOpened(id, expires) => {
                    entry.open.insert(id.clone(), expires);
                    if entry.remember(&id) {
                        kinds.push(Kind::Ask);
                    }
                }
                Mirror::AskEnded(id) => {
                    entry.open.remove(&id);
                    if entry.open.is_empty() && entry.stop_pending {
                        entry.stop_pending = false;
                        kinds.push(Kind::Stopped);
                    }
                }
                Mirror::Stopped(epoch, seq) => {
                    if entry.stops.insert((epoch, seq)) {
                        if entry.stops.len() > NOTIFIED_CAP {
                            entry.stops.clear();
                        }
                        // Held while an ask is open: the ask's push covers it.
                        if entry.open.is_empty() {
                            kinds.push(Kind::Stopped);
                        } else {
                            entry.stop_pending = true;
                        }
                    }
                }
                // Notified ids are kept, so a re-share does not repeat them.
                Mirror::Gap | Mirror::Gone => entry.lose_track(),
            }
            kinds
                .into_iter()
                .map(|kind| (kind, cause))
                .collect::<Vec<_>>()
        };
        for (kind, cause) in wanted {
            self.want(kind, cause);
        }
    }

    /// Whether a push of `kind` for `cause` is still due, and with which TTL:
    /// the session is still relayed with the same verified view, and for an
    /// ask one is still open (and not expired); for a stop none is open.
    fn due(&self, kind: Kind, cause: Cause) -> Option<u64> {
        let state = self.state();
        let session = state.sessions.get(&cause.session)?;
        if !session.live || session.generation != cause.generation {
            return None;
        }
        match kind {
            Kind::Ask => {
                let latest = session.open.values().copied().max_by_key(|expires| {
                    if *expires == 0 {
                        u64::MAX
                    } else {
                        *expires
                    }
                })?;
                if latest == 0 {
                    return Some(ASK_TTL_SECS);
                }
                let left = latest.saturating_sub(now_ms()) / 1000;
                (left > 0).then(|| left.min(ASK_TTL_SECS))
            }
            Kind::Stopped => session.open.is_empty().then_some(STOPPED_TTL_SECS),
            // Tests are sent directly by `test`, never through a session.
            Kind::Test => None,
        }
    }

    /// Every subscribed device allowed in this workspace now.
    fn recipients(&self) -> Vec<(String, PushSubscription)> {
        self.registry
            .list()
            .unwrap_or_default()
            .into_iter()
            .filter(|record| self.registry.standing(&record.id).is_ok())
            .filter_map(|record| record.push.map(|push| (record.id, push)))
            .collect()
    }

    fn want(self: &Arc<Self>, kind: Kind, cause: Cause) {
        for (device, push) in self.recipients() {
            let now = tokio::time::Instant::now();
            let mut state = self.state();
            let window = state.windows.entry((device.clone(), kind)).or_default();
            let open = window
                .last_sent
                .is_none_or(|last| now.duration_since(last) >= self.window);
            if open {
                window.last_sent = Some(now);
                window.pending.clear();
                drop(state);
                self.dispatch(device, push.rev, push.key_id, kind, cause);
            } else {
                if !window.pending.contains(&cause) {
                    window.pending.push(cause);
                }
                if !window.trailing {
                    window.trailing = true;
                    let at = window.last_sent.unwrap_or(now) + self.window;
                    drop(state);
                    let sender = self.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep_until(at).await;
                        sender.trailing(device, kind);
                    });
                }
            }
        }
    }

    /// The end of a window with something left over: send once more, for
    /// the first cause that is still due now.
    fn trailing(self: &Arc<Self>, device: String, kind: Kind) {
        let pending = {
            let mut state = self.state();
            let window = state.windows.entry((device.clone(), kind)).or_default();
            window.trailing = false;
            std::mem::take(&mut window.pending)
        };
        let Some(cause) = pending
            .into_iter()
            .find(|cause| self.due(kind, *cause).is_some())
        else {
            return;
        };
        if let Some(push) = self.registry.push_of(&device) {
            self.state()
                .windows
                .entry((device.clone(), kind))
                .or_default()
                .last_sent = Some(tokio::time::Instant::now());
            self.dispatch(device, push.rev, push.key_id, kind, cause);
        }
    }

    /// Send one push to `device`, for the exact subscription `rev` made
    /// against `key_id`, for `cause`, if all of it still holds when it goes.
    fn dispatch(
        self: &Arc<Self>,
        device: String,
        rev: String,
        key_id: String,
        kind: Kind,
        cause: Cause,
    ) {
        let sender = self.clone();
        tokio::spawn(async move {
            let job = Job {
                device: &device,
                rev: &rev,
                key_id: &key_id,
                kind,
                cause,
            };
            let outcome = sender.send_checked(&job).await;
            let detail = match &outcome {
                Some((host, Ok(status))) => format!("{host} {status}"),
                Some((host, Err(error))) => format!("{host} {error}"),
                None => return,
            };
            sender.registry.audit("push_sent", Some(&device), &detail);
            if let Some((_, Ok(404 | 410))) = outcome {
                let _ = sender.registry.clear_push_rev(&device, &rev);
            }
        });
    }

    /// Whether `job` may still go now, with its TTL: the device's standing,
    /// the exact subscription revision and key, the current key, and the
    /// cause still due. Read fresh every time.
    fn still_valid(&self, job: &Job<'_>) -> Option<(PushSubscription, u64)> {
        self.registry.standing(job.device).ok()?;
        let push = self.registry.push_of(job.device)?;
        if push.rev != job.rev || push.key_id != job.key_id {
            return None;
        }
        let key = self.keys.current().ok().flatten()?;
        if key.id() != job.key_id {
            return None;
        }
        let ttl = self.due(job.kind, job.cause)?;
        Some((push, ttl))
    }

    /// Send one test notification to `device`'s saved subscription, through
    /// the same checks and delivery as a real one, at most once per
    /// [`TEST_EVERY`] per device.
    pub(crate) async fn test(&self, device: &str) -> TestOutcome {
        {
            let mut state = self.state();
            let now = tokio::time::Instant::now();
            if let Some(last) = state.tests.get(device) {
                let since = now.saturating_duration_since(*last);
                if since < TEST_EVERY {
                    return TestOutcome::TooSoon((TEST_EVERY - since).as_secs().max(1));
                }
            }
            state.tests.insert(device.to_string(), now);
        }
        if self.registry.standing(device).is_err() {
            return TestOutcome::NotAllowed;
        }
        let Some(push) = self.registry.push_of(device) else {
            return TestOutcome::NoSubscription;
        };
        let Some(key) = self.keys.current().ok().flatten() else {
            return TestOutcome::NoSubscription;
        };
        if key.id() != push.key_id {
            return TestOutcome::NoSubscription;
        }
        let Ok(host) = check_endpoint(&push.endpoint).map(|endpoint| endpoint.host) else {
            return TestOutcome::Failed;
        };
        let Ok((public, auth)) = check_keys(&push.p256dh, &push.auth) else {
            return TestOutcome::Failed;
        };
        let Ok(body) =
            crate::remote_push_crypto::encrypt(&public, &auth, Kind::Test.payload(), PADDED)
        else {
            return TestOutcome::Failed;
        };
        let audience = format!("https://{host}");
        let expires = now_ms() / 1000 + 12 * 60 * 60;
        let Ok(authorization) =
            crate::remote_push_crypto::vapid_authorization(key.pair(), &audience, expires)
        else {
            return TestOutcome::Failed;
        };
        let out = Outgoing {
            endpoint: push.endpoint.clone(),
            host: host.clone(),
            ttl_secs: TEST_TTL_SECS,
            urgency: Kind::Test.urgency(),
            authorization,
            body,
        };
        let rev = push.rev.clone();
        let recheck = || {
            self.registry.standing(device).is_ok()
                && self
                    .registry
                    .push_of(device)
                    .is_some_and(|current| current.rev == rev)
        };
        let outcome = deliver(&self.resolve, &self.post, out, &recheck).await;
        let detail = match &outcome {
            Ok(status) => format!("{host} {status}"),
            Err(error) => format!("{host} {error}"),
        };
        self.registry.audit("push_test", Some(device), &detail);
        match outcome {
            Ok(200..=299) => TestOutcome::Sent,
            Ok(404 | 410) => {
                let _ = self.registry.clear_push_rev(device, &push.rev);
                TestOutcome::Gone
            }
            Err(SendError::Withdrawn) => TestOutcome::NotAllowed,
            _ => TestOutcome::Failed,
        }
    }

    async fn send_checked(&self, job: &Job<'_>) -> Option<(String, Result<u16, SendError>)> {
        let (push, ttl_secs) = self.still_valid(job)?;
        let key = self.keys.current().ok().flatten()?;
        let host = check_endpoint(&push.endpoint).ok()?.host;
        let (public, auth) = check_keys(&push.p256dh, &push.auth).ok()?;
        let body =
            crate::remote_push_crypto::encrypt(&public, &auth, job.kind.payload(), PADDED).ok()?;
        let audience = format!("https://{host}");
        let expires = now_ms() / 1000 + 12 * 60 * 60;
        let authorization =
            crate::remote_push_crypto::vapid_authorization(key.pair(), &audience, expires).ok()?;
        let out = Outgoing {
            endpoint: push.endpoint,
            host: host.clone(),
            ttl_secs,
            urgency: job.kind.urgency(),
            authorization,
            body,
        };
        // Checked again after the name resolved, right before the request
        // starts: a revoke, replacement, rotation or the share ending during
        // resolution means nothing is sent.
        let recheck = || self.still_valid(job).is_some();
        match deliver(&self.resolve, &self.post, out, &recheck).await {
            Err(SendError::Withdrawn) => None,
            outcome => Some((host, outcome)),
        }
    }
}

/// One push to send.
struct Job<'a> {
    device: &'a str,
    rev: &'a str,
    key_id: &'a str,
    kind: Kind,
    cause: Cause,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use localpilot_rpc::Scope;
    use ring::agreement;

    type Sent = Arc<Mutex<Vec<(String, u64, &'static str, Vec<SocketAddr>)>>>;

    fn public_resolve() -> Resolve {
        Arc::new(|_host: String| Box::pin(async { Ok(vec!["142.250.1.1:443".parse().unwrap()]) }))
    }

    fn recording_post(sent: &Sent, status: u16) -> Post {
        let sent = sent.clone();
        Arc::new(move |out: Outgoing, addresses, _deadline| {
            sent.lock()
                .unwrap()
                .push((out.endpoint, out.ttl_secs, out.urgency, addresses));
            Box::pin(async move { Ok(status) })
        })
    }

    fn outgoing() -> Outgoing {
        Outgoing {
            endpoint: "https://fcm.googleapis.com/x".to_string(),
            host: "fcm.googleapis.com".to_string(),
            ttl_secs: 1,
            urgency: "high",
            authorization: String::new(),
            body: Vec::new(),
        }
    }

    fn answers(list: &'static [&'static str]) -> Resolve {
        Arc::new(move |_| {
            Box::pin(async move { Ok(list.iter().map(|a| a.parse().unwrap()).collect()) })
        })
    }

    #[tokio::test]
    async fn a_push_goes_only_to_pinned_public_addresses() {
        let sent: Sent = Arc::default();
        let post = recording_post(&sent, 201);
        for private in [
            &["10.0.0.1:443"][..],
            &["142.250.1.1:443", "127.0.0.1:443"][..],
            &["[::ffff:192.168.1.1]:443"][..],
            &["[fd00::1]:443"][..],
        ] {
            assert_eq!(
                deliver(&answers(private), &post, outgoing(), &|| true).await,
                Err(SendError::NotPublic),
                "{private:?}"
            );
        }
        assert_eq!(
            deliver(&answers(&[]), &post, outgoing(), &|| true).await,
            Err(SendError::Unresolved)
        );
        assert!(sent.lock().unwrap().is_empty(), "nothing was sent");
        assert_eq!(
            deliver(&answers(&["142.250.1.1:443"]), &post, outgoing(), &|| true).await,
            Ok(201)
        );
        let pinned = sent.lock().unwrap()[0].3.clone();
        assert_eq!(
            pinned,
            vec!["142.250.1.1:443".parse::<SocketAddr>().unwrap()]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_deadline_starts_before_the_name_is_resolved() {
        let sent: Sent = Arc::default();
        let slow: Resolve = Arc::new(|_| {
            Box::pin(async {
                tokio::time::sleep(SEND_DEADLINE + Duration::from_secs(1)).await;
                Ok(vec!["142.250.1.1:443".parse().unwrap()])
            })
        });
        assert_eq!(
            deliver(&slow, &recording_post(&sent, 201), outgoing(), &|| true).await,
            Err(SendError::TimedOut)
        );
        assert!(sent.lock().unwrap().is_empty());
    }

    struct Rig {
        sender: Arc<PushSender>,
        registry: Arc<DeviceRegistry>,
        keys: Arc<PushKeys>,
        device: String,
        sent: Sent,
        _dir: tempfile::TempDir,
    }

    fn rig(status: u16) -> Rig {
        rig_with(status, public_resolve())
    }

    fn rig_with(status: u16, resolve: Resolve) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let keys = Arc::new(PushKeys::at(dir.path().join("devices")));
        let key = keys.ensure().unwrap();
        let registry = Arc::new(DeviceRegistry::default());
        let (standing, _) = registry.add("phone", Scope::Observe, false).unwrap();
        let rng = ring::rand::SystemRandom::new();
        let browser =
            agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng).unwrap();
        let b64 = |bytes: &[u8]| {
            base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes)
        };
        registry
            .set_push(
                &standing.id,
                "https://fcm.googleapis.com/fcm/send/phone",
                &b64(browser.compute_public_key().unwrap().as_ref()),
                &b64(&[5u8; 16]),
                key.id(),
            )
            .unwrap();
        let sent: Sent = Arc::default();
        let sender = PushSender::with(
            registry.clone(),
            keys.clone(),
            resolve,
            recording_post(&sent, status),
            WINDOW,
        );
        Rig {
            sender,
            registry,
            keys,
            device: standing.id,
            sent,
            _dir: dir,
        }
    }

    /// Let spawned sends finish (time is paused; sleeping advances it).
    async fn settle() {
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(1)).await;
            tokio::task::yield_now().await;
        }
    }

    /// A session shared with a verified, empty baseline.
    fn shared(rig: &Rig) -> SessionId {
        let session = SessionId::new();
        rig.sender.observe(session, Mirror::Baseline(Vec::new()));
        session
    }

    fn kinds(rig: &Rig) -> Vec<&'static str> {
        rig.sent.lock().unwrap().iter().map(|sent| sent.2).collect()
    }

    #[tokio::test(start_paused = true)]
    async fn an_open_ask_is_notified_once_across_rebuilds_and_reconnects() {
        let rig = rig(201);
        let session = SessionId::new();
        let open = vec![("a1".to_string(), 0)];
        rig.sender.observe(session, Mirror::Baseline(open.clone()));
        settle().await;
        assert_eq!(kinds(&rig), vec!["high"]);
        assert_eq!(
            rig.sent.lock().unwrap()[0].1,
            ASK_TTL_SECS,
            "no expiry: 300 s"
        );
        // A snapshot rebuild, a relay reconnect and the same ask relayed again.
        rig.sender.observe(session, Mirror::Gone);
        rig.sender.observe(session, Mirror::Baseline(open));
        rig.sender
            .observe(session, Mirror::AskOpened("a1".to_string(), 0));
        tokio::time::sleep(WINDOW * 2).await;
        settle().await;
        assert_eq!(kinds(&rig).len(), 1, "never repeated");
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_coalesces_and_an_ask_still_open_gets_the_trailing_push() {
        let rig = rig(201);
        let session = shared(&rig);
        rig.sender
            .observe(session, Mirror::AskOpened("a1".to_string(), 0));
        settle().await;
        // Inside the window: a1 ends, a2 and a3 open; one trailing push.
        rig.sender
            .observe(session, Mirror::AskEnded("a1".to_string()));
        rig.sender
            .observe(session, Mirror::AskOpened("a2".to_string(), 0));
        rig.sender
            .observe(session, Mirror::AskOpened("a3".to_string(), 0));
        settle().await;
        assert_eq!(kinds(&rig).len(), 1);
        tokio::time::sleep(WINDOW).await;
        settle().await;
        assert_eq!(kinds(&rig), vec!["high", "high"], "the trailing push");
        // A window whose asks all ended sends no trailing push.
        rig.sender
            .observe(session, Mirror::AskOpened("a4".to_string(), 0));
        for ask in ["a2", "a3", "a4"] {
            rig.sender
                .observe(session, Mirror::AskEnded(ask.to_string()));
        }
        tokio::time::sleep(WINDOW * 2).await;
        settle().await;
        assert_eq!(kinds(&rig).iter().filter(|k| **k == "high").count(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_waits_for_open_asks_and_is_never_replayed() {
        let rig = rig(201);
        let session = shared(&rig);
        rig.sender
            .observe(session, Mirror::AskOpened("a1".to_string(), 0));
        settle().await;
        rig.sender
            .observe(session, Mirror::Stopped("e".to_string(), 7));
        settle().await;
        assert_eq!(kinds(&rig), vec!["high"], "held while the ask is open");
        rig.sender
            .observe(session, Mirror::AskEnded("a1".to_string()));
        settle().await;
        assert_eq!(kinds(&rig), vec!["high", "normal"]);
        assert_eq!(rig.sent.lock().unwrap()[1].1, STOPPED_TTL_SECS);
        tokio::time::sleep(WINDOW * 2).await;
        rig.sender
            .observe(session, Mirror::Stopped("e".to_string(), 7));
        settle().await;
        assert_eq!(kinds(&rig).len(), 2, "the same stop is not notified twice");
    }

    #[tokio::test(start_paused = true)]
    async fn a_trailing_stop_is_dropped_when_an_ask_opened_meanwhile() {
        let rig = rig(201);
        let session = shared(&rig);
        rig.sender
            .observe(session, Mirror::Stopped("e".to_string(), 1));
        settle().await;
        rig.sender
            .observe(session, Mirror::Stopped("e".to_string(), 2));
        settle().await;
        assert_eq!(
            kinds(&rig),
            vec!["normal"],
            "the second waits for the window"
        );
        // Before the window ends an ask opens: its push covers it.
        rig.sender
            .observe(session, Mirror::AskOpened("a".to_string(), 0));
        settle().await;
        tokio::time::sleep(WINDOW * 2).await;
        settle().await;
        assert_eq!(kinds(&rig), vec!["normal", "high"], "no trailing stop");
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_sent_after_revoke_replacement_rotation_or_expiry() {
        let rig1 = rig(201);
        assert!(rig1.registry.revoke(&rig1.device).unwrap());
        rig1.sender
            .observe(shared(&rig1), Mirror::AskOpened("a".to_string(), 0));
        settle().await;
        assert!(kinds(&rig1).is_empty(), "revoked");
        let rig2 = rig(201);
        rig2.keys.rotate(&DeviceRegistry::default()).unwrap();
        rig2.sender
            .observe(shared(&rig2), Mirror::AskOpened("a".to_string(), 0));
        settle().await;
        assert!(kinds(&rig2).is_empty(), "made against the old key");
        let rig3 = rig(201);
        let current = rig3.registry.push_of(&rig3.device).unwrap();
        let session = shared(&rig3);
        rig3.sender.dispatch(
            rig3.device.clone(),
            "not-the-current-rev".to_string(),
            current.key_id,
            Kind::Stopped,
            Cause {
                session,
                generation: 0,
            },
        );
        settle().await;
        assert!(kinds(&rig3).is_empty(), "replaced since the trigger");
        let rig4 = rig(201);
        rig4.sender
            .observe(shared(&rig4), Mirror::AskOpened("a".to_string(), 1));
        settle().await;
        assert!(kinds(&rig4).is_empty(), "already expired");
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_sent_for_a_session_that_stopped_being_shared() {
        // A stop inside the window, then the share ends: no trailing stop.
        let rig1 = rig(201);
        let session = shared(&rig1);
        rig1.sender
            .observe(session, Mirror::Stopped("e".to_string(), 1));
        settle().await;
        rig1.sender
            .observe(session, Mirror::Stopped("e".to_string(), 2));
        rig1.sender.observe(session, Mirror::Gone);
        tokio::time::sleep(WINDOW * 2).await;
        settle().await;
        assert_eq!(kinds(&rig1), vec!["normal"], "no trailing stop after Gone");
        // A stop whose send has not started when the share ends.
        let rig2 = rig(201);
        let session = shared(&rig2);
        rig2.sender
            .observe(session, Mirror::Stopped("e".to_string(), 1));
        rig2.sender.observe(session, Mirror::Gone);
        settle().await;
        assert!(kinds(&rig2).is_empty(), "the queued stop was dropped");
        // Ending one share leaves another one's pushes alone.
        let rig3 = rig(201);
        let (ending, staying) = (shared(&rig3), shared(&rig3));
        rig3.sender
            .observe(ending, Mirror::AskOpened("a".to_string(), 0));
        rig3.sender.observe(ending, Mirror::Gone);
        settle().await;
        assert!(kinds(&rig3).is_empty());
        // The dropped send still used the window; the other session's ask
        // goes with the trailing re-check.
        rig3.sender
            .observe(staying, Mirror::AskOpened("b".to_string(), 0));
        tokio::time::sleep(WINDOW).await;
        settle().await;
        assert_eq!(kinds(&rig3), vec!["high"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_mirror_gap_suspends_pushes_until_a_verified_baseline() {
        let rig = rig(201);
        let session = shared(&rig);
        rig.sender.observe(session, Mirror::Gap);
        rig.sender
            .observe(session, Mirror::AskOpened("a".to_string(), 0));
        rig.sender
            .observe(session, Mirror::Stopped("e".to_string(), 3));
        settle().await;
        assert!(
            kinds(&rig).is_empty(),
            "nothing between a gap and a baseline"
        );
        rig.sender
            .observe(session, Mirror::Baseline(vec![("a".to_string(), 0)]));
        settle().await;
        assert_eq!(kinds(&rig), vec!["high"], "the open ask, once verified");
    }

    /// A resolver that answers only when released.
    fn held_resolve(release: &Arc<tokio::sync::Notify>) -> Resolve {
        let release = release.clone();
        Arc::new(move |_| {
            let release = release.clone();
            Box::pin(async move {
                release.notified().await;
                Ok(vec!["142.250.1.1:443".parse().unwrap()])
            })
        })
    }

    #[tokio::test(start_paused = true)]
    async fn a_change_while_the_name_resolves_stops_the_request() {
        for change in ["revoke", "unsubscribe", "replace", "rotate", "gone"] {
            let release = Arc::new(tokio::sync::Notify::new());
            let rig = rig_with(201, held_resolve(&release));
            let session = shared(&rig);
            rig.sender
                .observe(session, Mirror::AskOpened("a".to_string(), 0));
            settle().await;
            match change {
                "revoke" => assert!(rig.registry.revoke(&rig.device).unwrap()),
                "unsubscribe" => assert!(rig.registry.clear_push(&rig.device).unwrap()),
                "replace" => {
                    let push = rig.registry.push_of(&rig.device).unwrap();
                    rig.registry
                        .set_push(
                            &rig.device,
                            &push.endpoint,
                            &push.p256dh,
                            &push.auth,
                            &push.key_id,
                        )
                        .unwrap();
                }
                "rotate" => {
                    rig.keys.rotate(&DeviceRegistry::default()).unwrap();
                }
                _ => rig.sender.observe(session, Mirror::Gone),
            }
            release.notify_one();
            settle().await;
            assert!(kinds(&rig).is_empty(), "{change}: no request started");
        }
        // Unchanged, the same held send goes.
        let release = Arc::new(tokio::sync::Notify::new());
        let rig = rig_with(201, held_resolve(&release));
        rig.sender
            .observe(shared(&rig), Mirror::AskOpened("a".to_string(), 0));
        settle().await;
        release.notify_one();
        settle().await;
        assert_eq!(kinds(&rig), vec!["high"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_gone_subscription_is_removed_and_audited_by_host_only() {
        let rig = rig(410);
        rig.sender
            .observe(shared(&rig), Mirror::AskOpened("a".to_string(), 0));
        settle().await;
        assert_eq!(kinds(&rig).len(), 1);
        assert!(
            rig.registry.push_of(&rig.device).is_none(),
            "410 removed it"
        );
        let audit = rig.registry.audit_lines().join("\n");
        assert!(
            audit.contains("push_sent") && audit.contains("fcm.googleapis.com 410"),
            "{audit}"
        );
        // `remote status` shows it as the last send.
        let (_, last) = rig.registry.last_audit("push_sent").unwrap();
        assert_eq!(last, "fcm.googleapis.com 410");
        let line = crate::remote_status::render_push(&crate::remote_status::PushStatus {
            enabled: true,
            subscribed: Ok(0),
            last: rig.registry.last_audit("push_sent"),
        });
        assert!(
            line.contains("on; 0 phones") && line.contains("fcm.googleapis.com 410"),
            "{line}"
        );
        assert!(!audit.contains("/fcm/send/phone"), "{audit}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_test_notification_goes_through_the_real_path_at_most_every_30_seconds() {
        let rig = rig(201);
        assert_eq!(rig.sender.test(&rig.device).await, TestOutcome::Sent);
        {
            let sent = rig.sent.lock().unwrap();
            assert_eq!(sent.len(), 1);
            assert_eq!(sent[0].0, "https://fcm.googleapis.com/fcm/send/phone");
            assert_eq!((sent[0].1, sent[0].2), (TEST_TTL_SECS, "normal"));
        }
        // Asked again at once: refused with the wait, nothing sent.
        match rig.sender.test(&rig.device).await {
            TestOutcome::TooSoon(secs) => assert!((1..=30).contains(&secs), "{secs}"),
            other => panic!("expected too soon, got {other:?}"),
        }
        assert_eq!(rig.sent.lock().unwrap().len(), 1);
        tokio::time::advance(TEST_EVERY).await;
        assert_eq!(rig.sender.test(&rig.device).await, TestOutcome::Sent);
        assert_eq!(rig.sent.lock().unwrap().len(), 2);
        let audit = rig.registry.audit_lines().join("\n");
        assert!(audit.contains("push_test"), "{audit}");
        assert!(!audit.contains("/fcm/send/phone"), "{audit}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_test_notification_reports_why_it_was_not_sent() {
        // No subscription.
        let r = rig(201);
        r.registry.clear_push(&r.device).unwrap();
        assert_eq!(r.sender.test(&r.device).await, TestOutcome::NoSubscription);
        // A subscription made for a key that was rotated away.
        let r = rig_with(201, public_resolve());
        // Rotated against another registry, so the stale subscription stays.
        r.keys.rotate(&DeviceRegistry::default()).unwrap();
        assert_eq!(r.sender.test(&r.device).await, TestOutcome::NoSubscription);
        // The push service says it is gone: removed.
        let r = rig(410);
        assert_eq!(r.sender.test(&r.device).await, TestOutcome::Gone);
        assert!(r.registry.push_of(&r.device).is_none());
        // A revoked device is never notified.
        let r = rig(201);
        r.registry.revoke_device(&r.device).unwrap();
        assert_eq!(r.sender.test(&r.device).await, TestOutcome::NotAllowed);
        assert!(r.sent.lock().unwrap().is_empty());
        // A push service that refuses it.
        let r = rig(500);
        assert_eq!(r.sender.test(&r.device).await, TestOutcome::Failed);
    }
}
