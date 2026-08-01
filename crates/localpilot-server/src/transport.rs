//! Cross-platform framed local transport.
//!
//! The public surface is identical on every platform: an [`Endpoint`] resolves
//! the OS-native address for a workspace, a [`Listener`] binds and accepts, and
//! the free function [`connect`] dials it. The concrete stream types differ by
//! platform (a Unix domain socket vs. a Windows named pipe) but every accepted
//! or dialled [`Conn`] is `AsyncRead + AsyncWrite + Unpin + Send`, so callers —
//! and the `localpilot-rpc` framing layered on top — never see the difference.

use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[cfg(unix)]
#[path = "transport_unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "transport_windows.rs"]
mod platform;

/// Environment override for the server endpoint address: an absolute socket
/// path on Unix or a `\\.\pipe\...` name on Windows.
pub const ENDPOINT_ENV: &str = "LOCALPILOT_SERVER_SOCKET";

/// The smaller of the two common `sockaddr_un::sun_path` capacities (104 on
/// macOS/BSD, 108 on Linux). Staying under the smaller keeps a resolved socket
/// path bindable on every Unix tier-1 target; one byte is reserved for the NUL
/// terminator.
#[cfg(unix)]
const MAX_UNIX_SOCKET_PATH: usize = 104;

/// Errors from the local transport.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TransportError {
    /// The resolved endpoint path does not fit the platform address limit.
    #[error("server endpoint address is too long for this platform: {len} bytes (limit {limit})")]
    EndpointTooLong {
        /// Length of the offending path, in bytes.
        len: usize,
        /// The platform limit that was exceeded.
        limit: usize,
    },
    /// No server is listening at the endpoint (the daemon is not running).
    #[error("no LocalPilot server is listening at the endpoint")]
    NotRunning,
    /// A server owns the endpoint but has no free instance to accept right now
    /// (Windows `ERROR_PIPE_BUSY`); the caller should retry.
    #[error("the server endpoint is busy: all pipe instances are in use")]
    Busy,
    /// An underlying transport I/O error.
    #[error("server transport i/o error: {0}")]
    Io(#[from] std::io::Error),
}

/// A resolved server endpoint address.
///
/// Deterministic per workspace: the same canonical workspace root always maps
/// to the same address, and two different roots derive different addresses, so
/// concurrent workspaces do not collide. No time or randomness enters the
/// derivation.
#[derive(Debug, Clone)]
pub struct Endpoint {
    #[cfg(unix)]
    socket: PathBuf,
    #[cfg(windows)]
    pipe: String,
}

impl Endpoint {
    /// Resolve the endpoint for a workspace root.
    ///
    /// Honours the [`ENDPOINT_ENV`] override when it is set and non-empty;
    /// otherwise derives an OS-native address under the platform runtime
    /// directory from a short, stable hash of the canonical workspace root.
    ///
    /// # Errors
    /// [`TransportError::EndpointTooLong`] when the resolved Unix socket path
    /// would exceed the platform `sun_path` limit.
    pub fn resolve(workspace_root: &Path) -> Result<Self, TransportError> {
        if let Some(addr) = env_override() {
            return Self::from_addr_checked(&addr);
        }
        let key = workspace_key(workspace_root);
        #[cfg(unix)]
        {
            let dir = unix_runtime_dir();
            let path = dir.join(format!("localpilot-{key}.sock"));
            Self::from_path_checked(path)
        }
        #[cfg(windows)]
        {
            Ok(Self {
                pipe: format!(r"\\.\pipe\localpilot-{key}"),
            })
        }
    }

    /// Build an endpoint from an explicit raw OS address — a filesystem path on
    /// Unix or a `\\.\pipe\...` name on Windows. Used for the environment
    /// override and for tests that need a unique, isolated endpoint.
    ///
    /// Unlike [`Endpoint::resolve`] this performs no length validation.
    #[must_use]
    pub fn from_addr(addr: &str) -> Self {
        #[cfg(unix)]
        {
            Self {
                socket: PathBuf::from(addr),
            }
        }
        #[cfg(windows)]
        {
            Self {
                pipe: addr.to_string(),
            }
        }
    }

    fn from_addr_checked(addr: &str) -> Result<Self, TransportError> {
        #[cfg(unix)]
        {
            Self::from_path_checked(PathBuf::from(addr))
        }
        #[cfg(windows)]
        {
            Ok(Self {
                pipe: addr.to_string(),
            })
        }
    }

    #[cfg(unix)]
    fn from_path_checked(socket: PathBuf) -> Result<Self, TransportError> {
        use std::os::unix::ffi::OsStrExt;
        let len = socket.as_os_str().as_bytes().len();
        // Reserve one byte for the NUL terminator the kernel appends.
        if len >= MAX_UNIX_SOCKET_PATH {
            return Err(TransportError::EndpointTooLong {
                len,
                limit: MAX_UNIX_SOCKET_PATH,
            });
        }
        Ok(Self { socket })
    }

    /// The Unix domain socket path this endpoint binds/dials.
    #[cfg(unix)]
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    /// The path of the exclusivity lock file that sits next to the socket.
    #[cfg(unix)]
    #[must_use]
    pub fn lock_path(&self) -> PathBuf {
        let mut name = self.socket.as_os_str().to_os_string();
        name.push(".lock");
        PathBuf::from(name)
    }

    /// The Windows named-pipe path this endpoint binds/dials.
    #[cfg(windows)]
    #[must_use]
    pub fn pipe_name(&self) -> &str {
        &self.pipe
    }

    /// A human-readable rendering of the endpoint address, for logs and errors.
    #[must_use]
    pub fn display(&self) -> String {
        #[cfg(unix)]
        {
            self.socket.display().to_string()
        }
        #[cfg(windows)]
        {
            self.pipe.clone()
        }
    }
}

/// A connected local stream, either accepted by a [`Listener`] or dialled by
/// [`connect`]. Uniform across platforms: `AsyncRead + AsyncWrite + Unpin +
/// Send`.
#[derive(Debug)]
pub struct Conn(platform::RawConn);

impl Conn {
    fn new(raw: platform::RawConn) -> Self {
        Self(raw)
    }
}

// `platform::RawConn` is `Unpin` on every platform, so `Pin::get_mut` and
// `Pin::new` here are the safe, `Unpin`-gated calls — no `unsafe` projection.
impl AsyncRead for Conn {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl AsyncWrite for Conn {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}

/// A bound local listener that accepts one [`Conn`] at a time.
#[derive(Debug)]
pub struct Listener(platform::Listener);

impl Listener {
    /// Bind the listener at `endpoint`.
    ///
    /// On Windows this claims the first pipe instance, which is also the
    /// singleton the daemon layer relies on; a second bind of a live endpoint
    /// fails. On Unix this binds the socket file and restricts it to the owner
    /// (`0600`); the daemon layer owns exclusivity via a separate lock file.
    ///
    /// # Errors
    /// [`TransportError::Io`] when the OS bind fails (e.g. the address is in
    /// use, or the Windows first instance is already claimed).
    pub fn bind(endpoint: &Endpoint) -> Result<Self, TransportError> {
        Ok(Self(platform::Listener::bind(endpoint)?))
    }

    /// Accept the next inbound connection.
    ///
    /// # Errors
    /// The underlying accept error.
    pub async fn accept(&self) -> std::io::Result<Conn> {
        Ok(Conn::new(self.0.accept().await?))
    }
}

/// Dial the server at `endpoint`.
///
/// # Errors
/// - [`TransportError::NotRunning`] when nothing is listening.
/// - [`TransportError::Busy`] when a server owns the endpoint but had no free
///   instance to accept within the bounded busy-retry window (Windows only).
/// - [`TransportError::Io`] for any other transport failure.
pub async fn connect(endpoint: &Endpoint) -> Result<Conn, TransportError> {
    Ok(Conn::new(platform::connect(endpoint).await?))
}

/// Whether a [`Listener::bind`] error means another live process already owns
/// the endpoint's singleton — on Windows, the first-pipe-instance claim failed
/// with `ERROR_ACCESS_DENIED`. (On Unix exclusivity is a separate lock file, so
/// this classification is not needed.)
#[cfg(windows)]
pub(crate) fn bind_lost_to_owner(error: &TransportError) -> bool {
    const ERROR_ACCESS_DENIED: i32 = 5;
    matches!(error, TransportError::Io(inner) if inner.raw_os_error() == Some(ERROR_ACCESS_DENIED))
}

/// Whether a bind error is "address already in use" — on Unix, a stale socket
/// file left by a crashed daemon that the lock holder may now reap.
#[cfg(unix)]
pub(crate) fn is_addr_in_use(error: &TransportError) -> bool {
    matches!(error, TransportError::Io(inner) if inner.kind() == std::io::ErrorKind::AddrInUse)
}

fn env_override() -> Option<String> {
    match std::env::var(ENDPOINT_ENV) {
        Ok(value) if !value.trim().is_empty() => Some(value),
        _ => None,
    }
}

/// A short, stable hex key derived from the canonical workspace root.
///
/// FNV-1a over the canonical path bytes: deterministic across processes and
/// platform versions (no time, no randomness, no external hash crate), and wide
/// enough that the handful of workspaces on one machine do not collide.
fn workspace_key(workspace_root: &Path) -> String {
    let canonical = std::fs::canonicalize(workspace_root).unwrap_or_else(|_| workspace_root.into());
    let bytes = canonical.to_string_lossy();
    format!("{:016x}", fnv1a_64(bytes.as_bytes()))
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// The short-lived runtime directory to place the socket under, preferring
/// `$XDG_RUNTIME_DIR`, then `$TMPDIR`, then `/tmp`. Kept short so the resolved
/// `<dir>/localpilot-<key>.sock` stays within the `sun_path` limit.
#[cfg(unix)]
fn unix_runtime_dir() -> PathBuf {
    for var in ["XDG_RUNTIME_DIR", "TMPDIR"] {
        if let Some(dir) = std::env::var_os(var) {
            if !dir.is_empty() {
                return PathBuf::from(dir);
            }
        }
    }
    PathBuf::from("/tmp")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::io::AsyncReadExt;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A unique, isolated endpoint for one test.
    fn unique_endpoint() -> Endpoint {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        #[cfg(windows)]
        {
            Endpoint::from_addr(&format!(r"\\.\pipe\localpilot-test-{pid}-{n}"))
        }
        #[cfg(unix)]
        {
            let dir = super::unix_runtime_dir();
            Endpoint::from_addr(&format!("{}/lp-t-{pid}-{n}.sock", dir.display()))
        }
    }

    async fn read_record<R: AsyncReadExt + Unpin>(reader: &mut R) -> serde_json::Value {
        let mut framing = localpilot_rpc::LineFraming::default();
        let mut chunk = [0u8; 1024];
        loop {
            let read = reader.read(&mut chunk).await.unwrap();
            assert!(read > 0, "connection closed before a full record arrived");
            let records = framing.push(&chunk[..read]).unwrap();
            if let Some(first) = records.into_iter().next() {
                return serde_json::from_slice(&first).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn round_trips_framed_json_both_directions() {
        let endpoint = unique_endpoint();
        let listener = Listener::bind(&endpoint).unwrap();

        let server = tokio::spawn(async move {
            let conn = listener.accept().await.unwrap();
            let (mut r, mut w) = tokio::io::split(conn);
            // Client -> server.
            let got = read_record(&mut r).await;
            assert_eq!(got, serde_json::json!({"from": "client"}));
            // Server -> client.
            crate::wire::write_record(&mut w, &serde_json::json!({"from": "server"}))
                .await
                .unwrap();
        });

        let conn = connect(&endpoint).await.unwrap();
        let (mut r, mut w) = tokio::io::split(conn);
        crate::wire::write_record(&mut w, &serde_json::json!({"from": "client"}))
            .await
            .unwrap();
        let got = read_record(&mut r).await;
        assert_eq!(got, serde_json::json!({"from": "server"}));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn echo_serve_loop_round_trips() {
        let endpoint = unique_endpoint();
        let listener = Listener::bind(&endpoint).unwrap();

        let server = tokio::spawn(async move {
            let conn = listener.accept().await.unwrap();
            crate::wire::serve_echo(conn).await.unwrap();
        });

        let conn = connect(&endpoint).await.unwrap();
        let (read, mut write) = tokio::io::split(conn);
        let mut reader = localpilot_rpc::JsonRecordReader::new(read);

        // A plain record echoes verbatim.
        crate::wire::write_record(&mut write, &serde_json::json!({"hello": 1}))
            .await
            .unwrap();
        assert_eq!(
            reader.next().await.unwrap(),
            Some(serde_json::json!({"hello": 1}))
        );

        // A ping gets a pong carrying the ping payload.
        crate::wire::write_record(&mut write, &serde_json::json!({"ping": 42}))
            .await
            .unwrap();
        assert_eq!(
            reader.next().await.unwrap(),
            Some(serde_json::json!({"pong": 42}))
        );

        drop(write);
        drop(reader);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn connect_with_no_server_is_not_running() {
        let endpoint = unique_endpoint();
        let err = connect(&endpoint).await.unwrap_err();
        assert!(
            matches!(err, TransportError::NotRunning),
            "expected NotRunning, got {err:?}"
        );
    }

    #[test]
    fn workspace_key_is_stable_and_distinct() {
        let a1 = workspace_key(Path::new("/some/workspace/a"));
        let a2 = workspace_key(Path::new("/some/workspace/a"));
        let b = workspace_key(Path::new("/some/workspace/b"));
        assert_eq!(a1, a2, "same root must derive the same key");
        assert_ne!(a1, b, "different roots must derive different keys");
        assert_eq!(a1.len(), 16);
    }

    // --- Unix-only parity tests (authored here; run on a Unix box later) ---

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_socket_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let endpoint = unique_endpoint();
        let _listener = Listener::bind(&endpoint).unwrap();
        let mode = std::fs::metadata(endpoint.socket_path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "socket must be owner read/write only");
        let _ = std::fs::remove_file(endpoint.socket_path());
    }

    #[cfg(unix)]
    #[test]
    fn unix_overlong_socket_path_errors() {
        let long = format!("/tmp/{}.sock", "x".repeat(MAX_UNIX_SOCKET_PATH));
        // `from_path_checked` is the private length gate `resolve` runs; a child
        // module may reach an ancestor's private item.
        let err = Endpoint::from_path_checked(PathBuf::from(long)).unwrap_err();
        assert!(
            matches!(err, TransportError::EndpointTooLong { .. }),
            "expected EndpointTooLong, got {err:?}"
        );
    }
}
