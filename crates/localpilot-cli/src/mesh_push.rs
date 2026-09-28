//! Waking recipients after a post (spec §8b, P-3 and P-4).
//!
//! The mesh crate says which wakes are owed; this module dials them. All the
//! push I/O of one post shares one 2 s deadline, and a push still pending at
//! the deadline is dropped, which cancels its I/O, so nothing outlives it even
//! in the long-lived `mesh run`. A process-wide claim per (session,
//! recipient) keeps at most one push in flight for each recipient, with no
//! queue: a push that finds the claim held is recorded as `timeout` without an
//! attempt. Nothing depends on a push (P-7), so no outcome here can fail the
//! command that posted.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use localpilot_mesh::ops::push::{
    outcome_of_reply, PushJob, Transport, NO_PUSH_ENV, PUSH_DEADLINE, PUSH_REPLY_CAP,
};
use localpilot_mesh::Mesh;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

type Key = (String, String);

fn in_flight() -> &'static Mutex<HashSet<Key>> {
    static IN_FLIGHT: OnceLock<Mutex<HashSet<Key>>> = OnceLock::new();
    IN_FLIGHT.get_or_init(Mutex::default)
}

/// A recipient's in-flight slot, released when dropped: on completion, at the
/// deadline, and if the whole push is abandoned.
struct Claim(Key);

impl Claim {
    fn take(job: &PushJob) -> Option<Self> {
        let key = (job.session_id.clone(), job.to.clone());
        let mut set = in_flight().lock().ok()?;
        // `then`, not `then_some`: an eager `Self(key)` for a slot someone
        // else holds would be dropped at once, releasing their claim (and
        // deadlocking on this very lock).
        set.insert(key.clone()).then(|| Self(key))
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        if let Ok(mut set) = in_flight().lock() {
            set.remove(&self.0);
        }
    }
}

/// How many pushes are in flight for `session` in this process, for tests.
/// Filtered by session because tests share the process-wide set when they
/// run as threads of one process.
#[cfg(test)]
pub(crate) fn in_flight_count(session: &str) -> usize {
    in_flight()
        .lock()
        .map_or(0, |s| s.iter().filter(|(sid, _)| sid == session).count())
}

/// Send every wake owed for the messages appended through `mesh` since the
/// last call, and record each outcome in the sender's push facts.
pub(crate) async fn push_all(mesh: &Mesh) {
    let jobs = {
        let mesh = mesh.clone();
        tokio::task::spawn_blocking(move || mesh.take_push_jobs())
            .await
            .unwrap_or_default()
    };
    if jobs.is_empty() || std::env::var(NO_PUSH_ENV).as_deref() == Ok("1") {
        return;
    }
    send(mesh, jobs, PUSH_DEADLINE).await;
}

/// Dial `jobs` under one shared deadline, then record each outcome.
async fn send(mesh: &Mesh, jobs: Vec<PushJob>, within: std::time::Duration) {
    let deadline = tokio::time::Instant::now() + within;
    let mut set = tokio::task::JoinSet::new();
    for (i, job) in jobs.into_iter().enumerate() {
        set.spawn(async move {
            let outcome = match Claim::take(&job) {
                None => "timeout",
                Some(_claim) => tokio::time::timeout_at(deadline, exchange(&job))
                    .await
                    .unwrap_or("timeout"),
            };
            (i, job, outcome)
        });
    }
    let mut done = Vec::new();
    while let Some(r) = set.join_next().await {
        if let Ok(result) = r {
            done.push(result);
        }
    }
    // Recorded in the order the wakes were owed, as the reference does, not
    // the order they finished.
    done.sort_by_key(|(i, _, _)| *i);
    let mesh = mesh.clone();
    let _ = tokio::task::spawn_blocking(move || {
        for (_, job, outcome) in &done {
            // Each record on its own: one failure costs only that record.
            let _ = mesh.record_push_fact(job, outcome);
        }
    })
    .await;
}

/// One wake exchange (P-2), without a deadline of its own.
async fn exchange(job: &PushJob) -> &'static str {
    let line = job.request_line();
    match job.transport {
        Transport::Pipe => pipe(&job.address, &line).await,
        Transport::Unix => unix(&job.address, &line).await,
    }
}

#[cfg(windows)]
async fn pipe(address: &str, line: &[u8]) -> &'static str {
    match tokio::net::windows::named_pipe::ClientOptions::new().open(address) {
        Ok(mut conn) => talk(&mut conn, line).await,
        Err(_) => "failed",
    }
}

#[cfg(not(windows))]
async fn pipe(_address: &str, _line: &[u8]) -> &'static str {
    // Never reached: `dialable` offers `pipe` only on Windows.
    "failed"
}

#[cfg(unix)]
async fn unix(address: &str, line: &[u8]) -> &'static str {
    match tokio::net::UnixStream::connect(address).await {
        Ok(mut conn) => talk(&mut conn, line).await,
        Err(_) => "failed",
    }
}

#[cfg(not(unix))]
async fn unix(_address: &str, _line: &[u8]) -> &'static str {
    // Never reached: `dialable` offers `unix` only on Unix.
    "failed"
}

async fn talk<S: AsyncRead + AsyncWrite + Unpin>(conn: &mut S, line: &[u8]) -> &'static str {
    if conn.write_all(line).await.is_err() {
        return "failed";
    }
    let mut reply = Vec::new();
    let mut chunk = [0u8; 4096];
    while !reply.contains(&b'\n') && reply.len() < PUSH_REPLY_CAP {
        match conn.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => reply.extend_from_slice(&chunk[..n]),
            Err(_) => return "failed",
        }
    }
    outcome_of_reply(&reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// A test endpoint: records each request, and answers `reply` or, when
    /// it is `None`, holds the connection and never answers.
    struct Listener {
        address: String,
        transport: Transport,
        heard: Arc<Mutex<Vec<String>>>,
        _dir: Option<tempfile::TempDir>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
        mut conn: S,
        heard: Arc<Mutex<Vec<String>>>,
        reply: Option<&'static [u8]>,
    ) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        while !buf.contains(&b'\n') {
            match conn.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        heard
            .lock()
            .unwrap()
            .push(String::from_utf8_lossy(&buf).trim_end().to_owned());
        match reply {
            Some(r) => {
                let _ = conn.write_all(r).await;
            }
            None => std::future::pending::<()>().await,
        }
    }

    #[cfg(windows)]
    fn listen(reply: Option<&'static [u8]>) -> Listener {
        use tokio::net::windows::named_pipe::ServerOptions;
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let address = format!(r"\\.\pipe\lp-push-test-{}-{n}", std::process::id());
        let heard = Arc::new(Mutex::new(Vec::new()));
        let mut server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&address)
            .unwrap();
        let (h, a) = (heard.clone(), address.clone());
        let task = tokio::spawn(async move {
            loop {
                if server.connect().await.is_err() {
                    return;
                }
                let Ok(next) = ServerOptions::new().create(&a) else {
                    return;
                };
                tokio::spawn(serve(
                    std::mem::replace(&mut server, next),
                    h.clone(),
                    reply,
                ));
            }
        });
        Listener {
            address,
            transport: Transport::Pipe,
            heard,
            _dir: None,
            task,
        }
    }

    #[cfg(unix)]
    fn listen(reply: Option<&'static [u8]>) -> Listener {
        let dir = tempfile::tempdir().unwrap();
        let address = dir.path().join("wake.sock").to_string_lossy().into_owned();
        let heard = Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::UnixListener::bind(&address).unwrap();
        let h = heard.clone();
        let task = tokio::spawn(async move {
            while let Ok((conn, _)) = listener.accept().await {
                tokio::spawn(serve(conn, h.clone(), reply));
            }
        });
        Listener {
            address,
            transport: Transport::Unix,
            heard,
            _dir: Some(dir),
            task,
        }
    }

    fn job(l: &Listener, session: &str, to: &str, seq: usize) -> PushJob {
        PushJob {
            session_id: session.into(),
            sender: "localpilot".into(),
            to: to.into(),
            msg_id: format!("localpilot:{seq}"),
            generation: 1,
            transport: l.transport,
            address: l.address.clone(),
        }
    }

    fn facts(dir: &std::path::Path, session: &str) -> Vec<serde_json::Value> {
        let p = localpilot_mesh::Mailbox::at(dir).pushes(session, "localpilot");
        std::fs::read_to_string(p)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_live_endpoint_is_woken_and_the_outcome_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let mesh = Mesh::at(dir.path(), "flag");
        let l = listen(Some(b"{\"ok\":true}\n"));
        send(&mesh, vec![job(&l, "s-live", "claude", 1)], PUSH_DEADLINE).await;
        let heard = l.heard.lock().unwrap().clone();
        assert_eq!(heard.len(), 1);
        let req: serde_json::Value = serde_json::from_str(&heard[0]).unwrap();
        assert_eq!(req["op"], "wake");
        assert_eq!(req["from"], "localpilot");
        assert_eq!(facts(dir.path(), "s-live")[0]["outcome"], "sent");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_silent_endpoint_costs_each_post_the_deadline_and_leaves_nothing_in_flight() {
        // The long-lived writer's bound (P-4): 50 posts in a row to an
        // endpoint that never answers. Each round ends at its deadline, the
        // pending I/O is cancelled, and the claim is released every time, so
        // every round makes a real attempt.
        let dir = tempfile::tempdir().unwrap();
        let mesh = Mesh::at(dir.path(), "flag");
        let l = listen(None);
        let within = Duration::from_millis(150);
        for i in 1..=50 {
            let t0 = Instant::now();
            send(&mesh, vec![job(&l, "s-silent", "claude", i)], within).await;
            let took = t0.elapsed();
            // The deadline bounds only the push I/O; recording the outcome
            // is file I/O on top (P-4), which a loaded CI runner can stretch
            // past a second. A push that outlived its deadline would never
            // return from this silent endpoint, so the bound still catches it.
            assert!(
                took < within + Duration::from_secs(10),
                "round {i} took {took:?}"
            );
            assert_eq!(in_flight_count("s-silent"), 0, "round {i}");
        }
        let f = facts(dir.path(), "s-silent");
        assert_eq!(f.len(), 50);
        assert!(f.iter().all(|x| x["outcome"] == "timeout"));
        assert_eq!(l.heard.lock().unwrap().len(), 50);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_recipient_already_in_flight_gets_no_second_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let mesh = Mesh::at(dir.path(), "flag");
        let l = listen(Some(b"{\"ok\":true}\n"));
        let held = Claim::take(&job(&l, "s-held", "claude", 1)).unwrap();
        send(&mesh, vec![job(&l, "s-held", "claude", 2)], PUSH_DEADLINE).await;
        assert!(l.heard.lock().unwrap().is_empty());
        assert_eq!(facts(dir.path(), "s-held")[0]["outcome"], "timeout");
        drop(held);
        send(&mesh, vec![job(&l, "s-held", "claude", 3)], PUSH_DEADLINE).await;
        assert_eq!(l.heard.lock().unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn outcomes_are_recorded_in_the_order_the_wakes_were_owed() {
        // The silent endpoint finishes last, but is recorded first.
        let dir = tempfile::tempdir().unwrap();
        let mesh = Mesh::at(dir.path(), "flag");
        let (quiet, ok) = (listen(None), listen(Some(b"{\"ok\":true}\n")));
        send(
            &mesh,
            vec![
                job(&quiet, "s-order", "claude", 1),
                job(&ok, "s-order", "codex", 1),
            ],
            Duration::from_millis(300),
        )
        .await;
        let f = facts(dir.path(), "s-order");
        assert_eq!(
            f.iter()
                .map(|x| (x["to"].as_str().unwrap(), x["outcome"].as_str().unwrap()))
                .collect::<Vec<_>>(),
            [("claude", "timeout"), ("codex", "sent")]
        );
    }
}
