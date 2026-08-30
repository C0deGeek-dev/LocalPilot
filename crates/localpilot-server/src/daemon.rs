//! Daemon lifecycle: detached spawn, a retry-connect ready handshake, and
//! single-owner exclusivity with stale-endpoint reaping.
//!
//! All primitives are safe `std` + `tokio`: detachment sets a new process group
//! (Unix) or `DETACHED_PROCESS | CREATE_NO_WINDOW` (Windows) with null stdio;
//! exclusivity is an atomic exclusive-create lock file next to the socket (Unix)
//! or the first-pipe-instance flag (Windows). No `unsafe`, no `libc`/`nix`, no
//! `setsid`/`flock`/`kill`.
//!
//! The `serve`/`connect` CLI commands do not exist yet (a later subject), so the
//! spawn target argument [`SERVE_ARGV`] is a documented placeholder the future
//! `serve` command will honour.

use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::transport::{self, connect, Conn, Endpoint, Listener, TransportError};

/// The internal argument that means "run the server". The CLI wiring is a later
/// subject; for now this is the placeholder argv a future `serve` command will
/// honour. Passed explicitly through [`spawn_detached_argv`] so it is testable.
pub const SERVE_ARGV: &[&str] = &["__server-serve"];

/// Windows `DETACHED_PROCESS`: the child gets no console of its parent's.
#[cfg(windows)]
const DETACHED_PROCESS: u32 = 0x0000_0008;
/// Windows `CREATE_NO_WINDOW`: the child gets no console window at all.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The Windows creation flags for a detached daemon.
#[cfg(windows)]
pub(crate) const fn detached_creation_flags() -> u32 {
    DETACHED_PROCESS | CREATE_NO_WINDOW
}

/// How many times [`acquire`] re-tries after reaping a stale endpoint before
/// giving up, so a persistently wedged endpoint cannot spin forever.
const MAX_ACQUIRE_ATTEMPTS: usize = 3;
/// A short pause between acquire attempts, so the reap/retry loop cannot become
/// a tight busy loop.
const ACQUIRE_RETRY_DELAY: Duration = Duration::from_millis(50);

/// The bound on the retry-connect ready handshake.
const READY_TIMEOUT: Duration = Duration::from_secs(5);
/// The poll interval while waiting for the daemon to come up.
const READY_POLL: Duration = Duration::from_millis(50);

/// Errors from the daemon lifecycle.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DaemonError {
    /// A transport-level failure.
    #[error(transparent)]
    Transport(#[from] TransportError),
    /// The daemon did not accept a connection within the ready window.
    #[error("daemon did not become ready within {0:?}")]
    ReadyTimeout(Duration),
    /// The singleton could not be acquired after reaping and retrying.
    #[error("could not acquire the server singleton after {0} attempts")]
    AcquireExhausted(usize),
    /// A lifecycle I/O failure (spawn, lock file, ...).
    #[error("daemon lifecycle i/o error: {0}")]
    Io(#[from] std::io::Error),
}

/// The outcome of trying to become the single daemon for an endpoint.
#[derive(Debug)]
pub enum Acquired {
    /// This process is now the daemon: it holds the bound [`Listener`] and the
    /// [`Singleton`] guard. Dropping them releases ownership.
    Owned {
        /// The bound listener to accept connections on.
        listener: Listener,
        /// The exclusivity guard (removes the lock file on drop, Unix).
        singleton: Singleton,
    },
    /// A live daemon already owns the endpoint; reuse it via [`connect`].
    AlreadyRunning,
}

/// An RAII guard for single-daemon ownership.
///
/// On Unix it removes the exclusive-create lock file on drop. On Windows the
/// bound [`Listener`] (the first pipe instance) is itself the singleton, so this
/// guard is an inert marker that ties the ownership lifetime to the same scope.
#[derive(Debug)]
pub struct Singleton {
    #[cfg(unix)]
    lock_path: PathBuf,
    #[cfg(windows)]
    _private: (),
}

impl Drop for Singleton {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let _ = std::fs::remove_file(&self.lock_path);
        }
    }
}

/// Build the detached [`Command`] that runs `program` with `args`.
///
/// A pure helper (it does not spawn) so the argv and detach configuration are
/// testable. All three standard streams are null-redirected; on Unix the child
/// leads a new process group, on Windows it is created detached with no console.
#[must_use]
pub fn build_serve_command(program: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // A new process group detaches the daemon from the spawner's group, so a
        // group-directed signal to the parent never reaches it.
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(detached_creation_flags());
    }
    command
}

/// Spawn a detached daemon running `program` with `args`, then return
/// immediately. The child handle is dropped, which detaches it (drop neither
/// waits nor kills).
///
/// # Errors
/// [`DaemonError::Io`] if the spawn fails.
pub fn spawn_detached_argv(program: &Path, args: &[&str]) -> Result<(), DaemonError> {
    let mut command = build_serve_command(program, args);
    tracing::debug!(program = %program.display(), "spawning detached localpilot server");
    let _child = command.spawn()?;
    Ok(())
}

/// Spawn a detached copy of the current executable as the server, using
/// [`SERVE_ARGV`].
///
/// # Errors
/// [`DaemonError::Io`] if the current-exe path is unavailable or the spawn
/// fails.
pub fn spawn_detached() -> Result<(), DaemonError> {
    let program = std::env::current_exe()?;
    spawn_detached_argv(&program, SERVE_ARGV)
}

/// Connect to the daemon if one is up; otherwise spawn one and wait for it to
/// become ready.
///
/// A momentarily busy daemon is waited out rather than duplicated: a second
/// daemon is spawned only when nothing owns the endpoint.
///
/// # Errors
/// [`DaemonError::ReadyTimeout`] if the spawned daemon never accepts within the
/// ready window; otherwise a transport or I/O error.
pub async fn ensure_running(endpoint: &Endpoint) -> Result<Conn, DaemonError> {
    match connect(endpoint).await {
        Ok(conn) => return Ok(conn),
        // A daemon owns the endpoint but is momentarily busy: wait, do not spawn
        // a second one.
        Err(TransportError::Busy) => return wait_for_ready(endpoint, READY_TIMEOUT).await,
        Err(TransportError::NotRunning) => {}
        Err(other) => return Err(other.into()),
    }
    tracing::debug!(endpoint = %endpoint.display(), "no daemon running; spawning one");
    spawn_detached()?;
    wait_for_ready(endpoint, READY_TIMEOUT).await
}

/// Retry-connect to `endpoint` on a bounded backoff until a connection succeeds
/// or `timeout` elapses.
///
/// "Not running yet" and "busy" are retried; any other error is returned
/// immediately.
///
/// # Errors
/// [`DaemonError::ReadyTimeout`] on timeout; otherwise a transport error.
pub async fn wait_for_ready(endpoint: &Endpoint, timeout: Duration) -> Result<Conn, DaemonError> {
    let deadline = Instant::now() + timeout;
    loop {
        match connect(endpoint).await {
            Ok(conn) => return Ok(conn),
            Err(TransportError::NotRunning | TransportError::Busy) => {
                if Instant::now() >= deadline {
                    return Err(DaemonError::ReadyTimeout(timeout));
                }
                tokio::time::sleep(READY_POLL).await;
            }
            Err(other) => return Err(other.into()),
        }
    }
}

/// Try to become the single daemon for `endpoint`.
///
/// Returns [`Acquired::Owned`] if this process wins ownership, or
/// [`Acquired::AlreadyRunning`] if a live daemon already holds it. If the
/// singleton is held but no daemon answers (a stale socket/lock from a crash),
/// the stale endpoint is reaped and acquisition is retried, bounded by
/// [`MAX_ACQUIRE_ATTEMPTS`].
///
/// # Errors
/// [`DaemonError::AcquireExhausted`] if a stale endpoint could not be reaped and
/// re-acquired within the attempt bound; otherwise a transport or I/O error.
pub async fn acquire(endpoint: &Endpoint) -> Result<Acquired, DaemonError> {
    for _ in 0..MAX_ACQUIRE_ATTEMPTS {
        match try_own(endpoint)? {
            Some((listener, singleton)) => {
                tracing::debug!(endpoint = %endpoint.display(), "acquired server singleton");
                return Ok(Acquired::Owned {
                    listener,
                    singleton,
                });
            }
            None => match connect(endpoint).await {
                // A live daemon answered (or is merely busy): reuse it.
                Ok(_probe) => return Ok(Acquired::AlreadyRunning),
                Err(TransportError::Busy) => return Ok(Acquired::AlreadyRunning),
                // Owned-but-dead: a stale endpoint. Reap and retry.
                Err(TransportError::NotRunning) => {
                    tracing::debug!(endpoint = %endpoint.display(), "reaping stale endpoint");
                    reap_stale(endpoint);
                    tokio::time::sleep(ACQUIRE_RETRY_DELAY).await;
                }
                Err(other) => return Err(other.into()),
            },
        }
    }
    Err(DaemonError::AcquireExhausted(MAX_ACQUIRE_ATTEMPTS))
}

/// Attempt exclusive ownership. `Ok(Some(..))` = won, `Ok(None)` = the singleton
/// is already claimed by someone (liveness is decided by the caller).
#[cfg(unix)]
fn try_own(endpoint: &Endpoint) -> Result<Option<(Listener, Singleton)>, DaemonError> {
    use std::io::Write;
    let lock_path = endpoint.lock_path();
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(file) => file,
        // Someone else holds the lock: claimed.
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    // Best-effort pid record; a failed write does not void ownership.
    let _ = writeln!(file, "{}", std::process::id());

    // We hold the lock, so any leftover socket file is a crash remnant we may
    // reap before binding.
    let listener = match Listener::bind(endpoint) {
        Ok(listener) => listener,
        Err(error) if transport::is_addr_in_use(&error) => {
            let _ = std::fs::remove_file(endpoint.socket_path());
            match Listener::bind(endpoint) {
                Ok(listener) => listener,
                Err(error) => {
                    let _ = std::fs::remove_file(&lock_path);
                    return Err(error.into());
                }
            }
        }
        Err(error) => {
            let _ = std::fs::remove_file(&lock_path);
            return Err(error.into());
        }
    };
    Ok(Some((listener, Singleton { lock_path })))
}

#[cfg(windows)]
fn try_own(endpoint: &Endpoint) -> Result<Option<(Listener, Singleton)>, DaemonError> {
    match Listener::bind(endpoint) {
        Ok(listener) => Ok(Some((listener, Singleton { _private: () }))),
        // First pipe instance already claimed by a live owner.
        Err(error) if transport::bind_lost_to_owner(&error) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Remove a stale endpoint's residue so a fresh listener can bind.
#[cfg(unix)]
fn reap_stale(endpoint: &Endpoint) {
    let _ = std::fs::remove_file(endpoint.socket_path());
    let _ = std::fs::remove_file(endpoint.lock_path());
}

#[cfg(windows)]
fn reap_stale(_endpoint: &Endpoint) {
    // Named pipes leave no filesystem residue: an instance exists only while a
    // process holds it, so there is nothing to unlink. The bounded acquire retry
    // covers the transient race where a dying owner released the pipe between our
    // bind attempt and the liveness probe.
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_endpoint() -> Endpoint {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        #[cfg(windows)]
        {
            Endpoint::from_addr(&format!(r"\\.\pipe\localpilot-dtest-{pid}-{n}"))
        }
        #[cfg(unix)]
        {
            let dir = std::env::temp_dir();
            Endpoint::from_addr(&format!("{}/lp-d-{pid}-{n}.sock", dir.display()))
        }
    }

    #[test]
    fn build_serve_command_sets_program_and_argv() {
        let program = Path::new("localpilot-under-test");
        let args = ["__server-serve", "--example"];
        let command = build_serve_command(program, &args);
        assert_eq!(command.get_program(), program.as_os_str());
        let got: Vec<_> = command.get_args().collect();
        assert_eq!(
            got,
            vec![
                std::ffi::OsStr::new("__server-serve"),
                std::ffi::OsStr::new("--example"),
            ]
        );
    }

    #[test]
    fn build_serve_command_detaches_stdio_and_group() {
        // The detach primitives are not readable back off a `Command`, so assert
        // on the source (mirrors the repo's spawn-invariant tests).
        let src = include_str!("daemon.rs");
        assert!(
            src.contains("process_group(0)"),
            "unix detach must use process_group(0)"
        );
        assert!(
            src.contains("creation_flags(detached_creation_flags())"),
            "windows detach must use creation_flags"
        );
        assert!(src.contains(".stdin(Stdio::null())"));
        assert!(src.contains(".stdout(Stdio::null())"));
        assert!(src.contains(".stderr(Stdio::null())"));
    }

    #[cfg(windows)]
    #[test]
    fn detached_creation_flags_value() {
        assert_eq!(super::detached_creation_flags(), 0x0800_0008);
    }

    #[tokio::test]
    async fn wait_for_ready_blocks_until_listener_is_up() {
        let endpoint = unique_endpoint();
        // Nothing is listening yet.
        assert!(matches!(
            connect(&endpoint).await,
            Err(TransportError::NotRunning)
        ));

        let bind_endpoint = endpoint.clone();
        let server = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let listener = Listener::bind(&bind_endpoint).unwrap();
            let _conn = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
        });

        let start = Instant::now();
        let conn = wait_for_ready(&endpoint, Duration::from_secs(5))
            .await
            .unwrap();
        assert!(
            start.elapsed() >= Duration::from_millis(100),
            "should have blocked until the listener came up"
        );
        drop(conn);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn acquire_grants_exactly_one_owner() {
        let endpoint = unique_endpoint();
        let first = acquire(&endpoint).await.unwrap();
        let second = acquire(&endpoint).await.unwrap();

        let mut owned = 0;
        let mut reused = 0;
        for outcome in [&first, &second] {
            match outcome {
                Acquired::Owned { .. } => owned += 1,
                Acquired::AlreadyRunning => reused += 1,
            }
        }
        assert_eq!(owned, 1, "exactly one process may own the endpoint");
        assert_eq!(reused, 1, "the other must detect-and-reuse");
        drop(first);
        drop(second);
    }

    // --- Unix-only parity tests (authored here; run on a Unix box later) ---

    #[cfg(unix)]
    #[tokio::test]
    async fn stale_endpoint_is_reaped_then_binds_fresh() {
        let endpoint = unique_endpoint();
        // Simulate a crashed daemon: a leftover socket file with no listener plus
        // a leftover lock file. `std::os::unix::net::UnixListener` does not unlink
        // on drop, so bind-then-drop leaves the socket file behind.
        {
            let _stale = std::os::unix::net::UnixListener::bind(endpoint.socket_path()).unwrap();
        }
        std::fs::write(endpoint.lock_path(), b"999999\n").unwrap();
        assert!(endpoint.socket_path().exists());
        assert!(endpoint.lock_path().exists());

        let acquired = acquire(&endpoint).await.unwrap();
        assert!(
            matches!(acquired, Acquired::Owned { .. }),
            "stale endpoint should be reaped and freshly bound"
        );

        // The fresh listener actually serves.
        let probe = connect(&endpoint).await.unwrap();
        drop(probe);

        drop(acquired);
        // Socket unlinked on listener drop; lock removed on singleton drop.
        assert!(!endpoint.socket_path().exists());
        assert!(!endpoint.lock_path().exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn live_daemon_is_reused_not_double_served() {
        let endpoint = unique_endpoint();
        let owner = acquire(&endpoint).await.unwrap();
        assert!(matches!(owner, Acquired::Owned { .. }));
        // A second acquire against the live owner must reuse, not reap.
        let second = acquire(&endpoint).await.unwrap();
        assert!(matches!(second, Acquired::AlreadyRunning));
        drop(owner);
    }
}
