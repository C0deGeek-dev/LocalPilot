//! `mesh run --listen`: LocalPilot's own push endpoint (spec §8b, P-5..P-7).
//!
//! The engine registers a delivery endpoint from its own process and keeps
//! the token in memory only: it is never printed, put in the environment or
//! passed on a command line. A listener takes one wake per connection,
//! validates it (P-2, P-5), accepts the message it names and nudges the engine
//! loop, which otherwise still looks at the journal on its own (P-7).
//!
//! The registration is a 60 s lease renewed every 20 s by its own task, so a
//! long model turn cannot let it lapse. Renewal and the listener share one
//! lease (generation and token). If renewal is refused (another process now
//! holds the endpoint), listening stops and the engine carries on polling;
//! it never takes an endpoint back. On a clean exit the renewal task is
//! cancelled and awaited, the listener closed, and the endpoint retired with
//! the current token.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use localpilot_mesh::Mesh;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// The registration's lifetime, in seconds.
const LEASE_TTL: i64 = 60;
/// How often the lease is renewed.
const RENEW_EVERY: Duration = Duration::from_secs(20);
/// Test hook, not a user option: the renewal period in milliseconds.
const RENEW_TEST_ENV: &str = "LOCALPILOT_TEST_MESH_RENEW_MS";
/// Test hook, not a user option: the lease in seconds.
const TTL_TEST_ENV: &str = "LOCALPILOT_TEST_MESH_TTL_S";
/// The longest wake request read, and how long a connection may take to send it.
const REQUEST_CAP: usize = 65_536;
const REQUEST_DEADLINE: Duration = Duration::from_secs(2);

/// What renewal and the listener share.
#[derive(Debug, Clone)]
struct Lease {
    session_id: String,
    generation: i64,
    token: String,
}

/// The lease's length: 60 s, or the test hook's.
fn lease_ttl() -> i64 {
    std::env::var(TTL_TEST_ENV)
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(LEASE_TTL)
}

/// How close to the lease's end a renewal may still be tried: a quarter of
/// the lease, at most 5 s. Past that the record may already have expired, and
/// an expired endpoint is replaceable without a token, so a late renewal could
/// silently take back an endpoint this process no longer holds.
fn renew_guard(ttl: i64) -> Duration {
    Duration::from_millis(u64::try_from(ttl).unwrap_or(0) * 250).min(Duration::from_secs(5))
}

/// When a lease registered at `before` (read just before the registration
/// call) ends by this process's clock. The record's `expires_at` is whole
/// seconds from a clock read during the call and rounded down, so it can fall
/// up to a second before `before + ttl`; a second is taken off to stay on the
/// early side. (The renewal itself is decided under the lock anyway: see
/// `Mesh::renew_endpoint`.)
fn lease_end(before: tokio::time::Instant, ttl: i64) -> tokio::time::Instant {
    before
        + Duration::from_secs(u64::try_from(ttl).unwrap_or(0))
            .saturating_sub(Duration::from_secs(1))
}

/// A running endpoint. [`Listening::stop`] retires it.
pub(crate) struct Listening {
    role: String,
    lease: Arc<RwLock<Lease>>,
    cancel: CancellationToken,
    renew: JoinHandle<()>,
    serve: JoinHandle<()>,
    /// Whether this process still holds the endpoint.
    held: Arc<std::sync::atomic::AtomicBool>,
    _dir: Option<tempfile::TempDir>,
}

impl Listening {
    /// Register and start listening for `role` in session `sid`, nudging
    /// `wake` on each valid wake. `Err` says why not; the engine then runs on
    /// polling alone.
    pub(crate) async fn start(
        mesh: &Mesh,
        role: &str,
        sid: &str,
        wake: Arc<Notify>,
    ) -> Result<Self, String> {
        let (transport, address, bound, dir) = bind().map_err(|e| format!("cannot listen: {e}"))?;
        let ttl = lease_ttl();
        let before = tokio::time::Instant::now();
        let registered = {
            let (mesh, role, transport, address) = (
                mesh.clone(),
                role.to_owned(),
                transport.to_owned(),
                address.clone(),
            );
            tokio::task::spawn_blocking(move || {
                mesh.register_endpoint(&role, &transport, &address, Some(ttl), "")
            })
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?
        };
        let (generation, token) = registered.map_err(|out| out.stderr.trim().to_owned())?;
        let lease = Arc::new(RwLock::new(Lease {
            session_id: sid.to_owned(),
            generation,
            token,
        }));
        let cancel = CancellationToken::new();
        let held = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let serve = tokio::spawn(serve(
            bound,
            mesh.clone(),
            role.to_owned(),
            lease.clone(),
            wake,
            cancel.clone(),
        ));
        let renew = tokio::spawn(renew(
            mesh.clone(),
            role.to_owned(),
            transport,
            address.clone(),
            lease.clone(),
            held.clone(),
            cancel.clone(),
            lease_end(before, ttl),
        ));
        println!("LISTENING role={role} transport={transport} generation={generation}");
        Ok(Self {
            role: role.to_owned(),
            lease,
            cancel,
            renew,
            serve,
            held,
            _dir: dir,
        })
    }

    /// Cancel and await renewal, close the listener, then retire the endpoint
    /// with the current token (unless another process took it over).
    pub(crate) async fn stop(self, mesh: &Mesh) {
        self.cancel.cancel();
        let _ = self.renew.await;
        self.serve.abort();
        let _ = self.serve.await;
        if !self.held.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let token = self
            .lease
            .read()
            .map(|l| l.token.clone())
            .unwrap_or_default();
        let (mesh, role) = (mesh.clone(), self.role.clone());
        let _ = tokio::task::spawn_blocking(move || mesh.unregister_endpoint(&role, &token)).await;
    }
}

/// The renewal period: 20 s, or the test hook's.
fn renew_every() -> Duration {
    std::env::var(RENEW_TEST_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map_or(RENEW_EVERY, Duration::from_millis)
}

#[allow(clippy::too_many_arguments)]
async fn renew(
    mesh: Mesh,
    role: String,
    transport: &'static str,
    address: String,
    lease: Arc<RwLock<Lease>>,
    held: Arc<std::sync::atomic::AtomicBool>,
    cancel: CancellationToken,
    mut ends: tokio::time::Instant,
) {
    let (every, ttl) = (renew_every(), lease_ttl());
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(every) => {}
        }
        // Too late to renew what this process can prove it still holds: stop
        // rather than risk registering over an expired record.
        if tokio::time::Instant::now() + renew_guard(ttl) >= ends {
            held.store(false, std::sync::atomic::Ordering::SeqCst);
            println!("LISTEN_STOPPED role={role}: the lease ran out before it could be renewed");
            cancel.cancel();
            return;
        }
        let token = lease.read().map(|l| l.token.clone()).unwrap_or_default();
        let before = tokio::time::Instant::now();
        let r = {
            let (mesh, role, address) = (mesh.clone(), role.clone(), address.clone());
            tokio::task::spawn_blocking(move || {
                mesh.renew_endpoint(&role, transport, &address, Some(ttl), &token)
            })
            .await
        };
        match r {
            Ok(Ok(Ok((generation, token)))) => {
                if let Ok(mut l) = lease.write() {
                    l.generation = generation;
                    l.token = token;
                }
                ends = lease_end(before, ttl);
            }
            // Refused: the endpoint is no longer ours to renew. Never take it
            // back; stop listening and let the engine poll.
            Ok(Ok(Err(out))) => {
                held.store(false, std::sync::atomic::Ordering::SeqCst);
                println!("LISTEN_STOPPED role={role}: {}", out.stderr.trim());
                cancel.cancel();
                return;
            }
            Ok(Err(e)) => println!("WARN lease renewal failed, retrying: {e}"),
            Err(e) => println!("WARN lease renewal failed, retrying: {e}"),
        }
    }
}

/// Answer one connection: read one wake, check it, accept, reply.
async fn answer<S: AsyncRead + AsyncWrite + Unpin>(
    mut conn: S,
    mesh: Mesh,
    role: String,
    lease: Arc<RwLock<Lease>>,
    wake: Arc<Notify>,
) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let read = tokio::time::timeout(REQUEST_DEADLINE, async {
        while !buf.contains(&b'\n') && buf.len() < REQUEST_CAP {
            match conn.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
    })
    .await;
    if read.is_err() {
        return;
    }
    let snapshot = lease.read().map(|l| l.clone()).ok();
    let reply = match (snapshot, check(&buf, &role)) {
        (_, Err(code)) => refusal(code),
        (None, _) => refusal("wrong_session"),
        (Some(l), Ok(w)) if w.session_id != l.session_id => refusal("wrong_session"),
        (Some(l), Ok(w)) if w.generation != l.generation => refusal("stale_generation"),
        (Some(l), Ok(w)) => {
            // The receipt is this endpoint's claim to have the message; a
            // refused accept (say, a message not addressed here) still leaves
            // a valid wake, which only ever says "look".
            let (mesh, role) = (mesh.clone(), role.clone());
            let _ = tokio::task::spawn_blocking(move || {
                mesh.accept_with(&role, &w.msg_id, l.generation, &l.token)
            })
            .await;
            wake.notify_one();
            b"{\"ok\":true}\n".to_vec()
        }
    };
    let _ = conn.write_all(&reply).await;
}

fn refusal(code: &str) -> Vec<u8> {
    format!("{{\"ok\":false,\"reason\":\"{code}\"}}\n").into_bytes()
}

/// The fields of a well-formed wake addressed to `role`.
#[derive(Debug, PartialEq, Eq)]
struct Wake {
    session_id: String,
    generation: i64,
    msg_id: String,
}

/// Validate a request line (P-2) and its recipient (P-5); the refusal code
/// otherwise. Session and generation are checked against the lease after.
fn check(line: &[u8], role: &str) -> Result<Wake, &'static str> {
    let first = line.split(|b| *b == b'\n').next().unwrap_or_default();
    let Ok(Value::Object(req)) = serde_json::from_slice::<Value>(first) else {
        return Err("bad_request");
    };
    let text = |k: &str| req.get(k).and_then(Value::as_str).map(str::to_owned);
    let int = |k: &str| {
        req.get(k)
            .filter(|v| v.is_i64() || v.is_u64())
            .and_then(Value::as_i64)
    };
    let (
        Some(v),
        Some(op),
        Some(session_id),
        Some(to),
        Some(generation),
        Some(msg_id),
        Some(_from),
    ) = (
        int("v"),
        text("op"),
        text("session_id"),
        text("to"),
        int("generation"),
        text("msg_id"),
        text("from"),
    )
    else {
        return Err("bad_request");
    };
    if v != 1 {
        return Err("unsupported_version");
    }
    if op != "wake" {
        return Err("unsupported_op");
    }
    if to != role {
        return Err("wrong_role");
    }
    Ok(Wake {
        session_id,
        generation,
        msg_id,
    })
}

/// The pipe's name, and its first instance, created before registering so
/// that no wake finds nothing listening.
#[cfg(windows)]
type Bound = (String, tokio::net::windows::named_pipe::NamedPipeServer);

#[cfg(unix)]
type Bound = tokio::net::UnixListener;

/// Pick an address and bind it: (transport, address, listener, directory).
#[cfg(windows)]
fn bind() -> std::io::Result<(&'static str, String, Bound, Option<tempfile::TempDir>)> {
    use tokio::net::windows::named_pipe::ServerOptions;
    // Unique per run; the DACL, not the name, is the boundary (ADR-0189).
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let address = format!(r"\\.\pipe\localpilot-mesh-{}-{nanos:x}", std::process::id());
    let first = localpilot_winsec::owner_only_pipe(
        ServerOptions::new().first_pipe_instance(true),
        &address,
    )?;
    Ok(("pipe", address.clone(), (address, first), None))
}

#[cfg(unix)]
fn bind() -> std::io::Result<(&'static str, String, Bound, Option<tempfile::TempDir>)> {
    use std::os::unix::fs::PermissionsExt;
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    // A fresh private directory: 0700, ours, and not a link, so the socket's
    // own mode is not the only boundary (unix(7)).
    let dir = tempfile::Builder::new()
        .prefix("localpilot-mesh-")
        .tempdir_in(base)?;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))?;
    let address = dir.path().join("wake.sock").to_string_lossy().into_owned();
    let listener = tokio::net::UnixListener::bind(&address)?;
    Ok(("unix", address, listener, Some(dir)))
}

#[cfg(windows)]
async fn serve(
    (name, mut server): Bound,
    mesh: Mesh,
    role: String,
    lease: Arc<RwLock<Lease>>,
    wake: Arc<Notify>,
    cancel: CancellationToken,
) {
    use tokio::net::windows::named_pipe::ServerOptions;
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            r = server.connect() => if r.is_err() { return; },
        }
        // Every instance owner-only (ADR-0189).
        let Ok(next) = localpilot_winsec::owner_only_pipe(&ServerOptions::new(), &name) else {
            return;
        };
        let conn = std::mem::replace(&mut server, next);
        tokio::spawn(answer(
            conn,
            mesh.clone(),
            role.clone(),
            lease.clone(),
            wake.clone(),
        ));
    }
}

#[cfg(unix)]
async fn serve(
    listener: Bound,
    mesh: Mesh,
    role: String,
    lease: Arc<RwLock<Lease>>,
    wake: Arc<Notify>,
    cancel: CancellationToken,
) {
    // The peer's uid must be the uid that owns our private socket directory.
    let own = std::fs::metadata(
        listener
            .local_addr()
            .ok()
            .and_then(|a| {
                a.as_pathname()
                    .and_then(|p| p.parent())
                    .map(std::path::Path::to_path_buf)
            })
            .unwrap_or_default(),
    )
    .map(|m| std::os::unix::fs::MetadataExt::uid(&m))
    .ok();
    loop {
        let conn = tokio::select! {
            () = cancel.cancelled() => return,
            r = listener.accept() => match r { Ok((c, _)) => c, Err(_) => return },
        };
        let peer = conn.peer_cred().ok().map(|c| c.uid());
        if own.is_none() || peer != own {
            continue; // dropped: not our user, or not checkable
        }
        tokio::spawn(answer(
            conn,
            mesh.clone(),
            role.clone(),
            lease.clone(),
            wake.clone(),
        ));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn line(v: serde_json::Value) -> Vec<u8> {
        let mut l = v.to_string().into_bytes();
        l.push(b'\n');
        l
    }

    #[test]
    fn a_renewal_is_only_tried_well_inside_the_lease() {
        assert_eq!(renew_guard(60), Duration::from_secs(5));
        assert_eq!(renew_guard(1), Duration::from_millis(250));
        let t0 = tokio::time::Instant::now();
        assert_eq!(lease_end(t0, 60), t0 + Duration::from_secs(59));
        assert_eq!(lease_end(t0, 1), t0);
    }

    #[test]
    fn a_well_formed_wake_for_this_role_passes() {
        let w = check(
            &line(serde_json::json!({"v": 1, "op": "wake", "session_id": "s", "to": "localpilot", "generation": 3, "msg_id": "claude:4", "from": "claude"})),
            "localpilot",
        )
        .unwrap();
        assert_eq!(
            w,
            Wake {
                session_id: "s".into(),
                generation: 3,
                msg_id: "claude:4".into()
            }
        );
    }

    #[test]
    fn each_malformed_or_misaddressed_wake_gets_its_code() {
        let base = serde_json::json!({"v": 1, "op": "wake", "session_id": "s", "to": "localpilot", "generation": 3, "msg_id": "claude:4", "from": "claude"});
        let with = |k: &str, v: serde_json::Value| {
            let mut o = base.clone();
            o[k] = v;
            line(o)
        };
        let without = |k: &str| {
            let mut o = base.clone();
            o.as_object_mut().unwrap().remove(k);
            line(o)
        };
        let cases: Vec<(Vec<u8>, &str)> = vec![
            (b"not json\n".to_vec(), "bad_request"),
            (b"[1]\n".to_vec(), "bad_request"),
            (without("msg_id"), "bad_request"),
            (without("from"), "bad_request"),
            (with("generation", serde_json::json!("3")), "bad_request"),
            (with("generation", serde_json::json!(3.5)), "bad_request"),
            (with("v", serde_json::json!(true)), "bad_request"),
            (with("v", serde_json::json!(2)), "unsupported_version"),
            (with("op", serde_json::json!("post")), "unsupported_op"),
            (with("to", serde_json::json!("codex")), "wrong_role"),
        ];
        for (req, code) in cases {
            assert_eq!(
                check(&req, "localpilot"),
                Err(code),
                "{}",
                String::from_utf8_lossy(&req)
            );
        }
    }
}
