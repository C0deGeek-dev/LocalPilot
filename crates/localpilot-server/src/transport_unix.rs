//! Unix transport backend: a `tokio` Unix domain socket.
//!
//! Included only on `#[cfg(unix)]`. The socket file is restricted to the owner
//! (`0600`) at bind and unlinked when the listener drops. Exclusivity itself is
//! owned one layer up by the daemon lock file, not by this backend.

use std::io;
use std::path::PathBuf;

use tokio::net::{UnixListener, UnixStream};

use super::{Endpoint, TransportError};

/// The concrete accepted/dialled stream on Unix.
pub(super) type RawConn = UnixStream;

/// A bound Unix domain socket listener that unlinks its socket file on drop.
#[derive(Debug)]
pub(super) struct Listener {
    inner: UnixListener,
    path: PathBuf,
}

impl Listener {
    pub(super) fn bind(endpoint: &Endpoint) -> Result<Self, TransportError> {
        let path = endpoint.socket_path().to_path_buf();
        let inner = UnixListener::bind(&path)?;
        // Restrict the freshly created socket to the owner. `set_permissions`
        // with `PermissionsExt` is safe std; the brief window between bind and
        // chmod is acceptable for a per-user runtime socket.
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        if let Err(error) = std::fs::set_permissions(&path, perms) {
            let _ = std::fs::remove_file(&path);
            return Err(error.into());
        }
        Ok(Self { inner, path })
    }

    pub(super) async fn accept(&self) -> io::Result<RawConn> {
        let (stream, _addr) = self.inner.accept().await?;
        Ok(stream)
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // Best-effort unlink; the socket is ours and a leftover file would look
        // like a stale endpoint to the next daemon (which reaps it anyway).
        let _ = std::fs::remove_file(&self.path);
    }
}

pub(super) async fn connect(endpoint: &Endpoint) -> Result<RawConn, TransportError> {
    match UnixStream::connect(endpoint.socket_path()).await {
        Ok(stream) => Ok(stream),
        // No socket file, or a socket file with no listener behind it: in both
        // cases nothing is serving, so this is "not running", not a hard error.
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Err(TransportError::NotRunning)
        }
        Err(error) => Err(error.into()),
    }
}
