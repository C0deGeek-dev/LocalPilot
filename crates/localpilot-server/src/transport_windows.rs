//! Windows transport backend: a `tokio` named pipe.
//!
//! Included only on `#[cfg(windows)]`. The first pipe instance is created with
//! `first_pipe_instance(true)`, which doubles as the daemon singleton — a
//! second attempt to claim the first instance of a live pipe fails with
//! `ERROR_ACCESS_DENIED`. Each accept creates the next waiting instance before
//! returning the just-connected one, so a client never races a missing pipe.

use std::io;
use std::pin::Pin;
use std::sync::{Mutex, PoisonError};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use tokio::time::{sleep, Duration};

use super::{Endpoint, TransportError};

/// `ERROR_FILE_NOT_FOUND`: no pipe with this name exists — nothing is serving.
const ERROR_FILE_NOT_FOUND: i32 = 2;
/// `ERROR_PIPE_BUSY`: the pipe exists but every instance is in use right now.
const ERROR_PIPE_BUSY: i32 = 231;

/// Bounded busy-retry: a server that owns the pipe but has no free instance is
/// transiently busy, so retry briefly before giving up. ~1s total.
const BUSY_RETRY_ATTEMPTS: u32 = 20;
const BUSY_RETRY_DELAY: Duration = Duration::from_millis(50);

/// The concrete accepted (server) or dialled (client) stream on Windows.
#[derive(Debug)]
pub(super) enum RawConn {
    Server(NamedPipeServer),
    Client(NamedPipeClient),
}

// Both pipe halves are `Unpin`, so `self.get_mut()` and `Pin::new` are the
// safe, `Unpin`-gated calls — no `unsafe` projection.
impl AsyncRead for RawConn {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            RawConn::Server(server) => Pin::new(server).poll_read(cx, buf),
            RawConn::Client(client) => Pin::new(client).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for RawConn {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            RawConn::Server(server) => Pin::new(server).poll_write(cx, buf),
            RawConn::Client(client) => Pin::new(client).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            RawConn::Server(server) => Pin::new(server).poll_flush(cx),
            RawConn::Client(client) => Pin::new(client).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            RawConn::Server(server) => Pin::new(server).poll_shutdown(cx),
            RawConn::Client(client) => Pin::new(client).poll_shutdown(cx),
        }
    }
}

/// A named-pipe listener holding the next waiting server instance.
#[derive(Debug)]
pub(super) struct Listener {
    pipe: String,
    pending: Mutex<Option<NamedPipeServer>>,
}

impl Listener {
    pub(super) fn bind(endpoint: &Endpoint) -> Result<Self, TransportError> {
        let pipe = endpoint.pipe_name().to_string();
        // `first_pipe_instance(true)` is the singleton claim: a second bind of a
        // live pipe fails with ERROR_ACCESS_DENIED, which the daemon layer reads
        // as "already running".
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&pipe)?;
        Ok(Self {
            pipe,
            pending: Mutex::new(Some(server)),
        })
    }

    pub(super) async fn accept(&self) -> io::Result<RawConn> {
        let server = self.take_pending()?;
        server.connect().await?;
        // Create the next waiting instance BEFORE returning the connected one.
        let next = ServerOptions::new().create(&self.pipe)?;
        self.store_pending(next)?;
        Ok(RawConn::Server(server))
    }

    fn take_pending(&self) -> io::Result<NamedPipeServer> {
        let mut guard = self.pending.lock().map_err(poisoned)?;
        match guard.take() {
            Some(server) => Ok(server),
            // The one-waiting-instance invariant normally holds; recover by
            // creating a fresh instance rather than erroring.
            None => ServerOptions::new().create(&self.pipe),
        }
    }

    fn store_pending(&self, server: NamedPipeServer) -> io::Result<()> {
        let mut guard = self.pending.lock().map_err(poisoned)?;
        *guard = Some(server);
        Ok(())
    }
}

fn poisoned<T>(_: PoisonError<T>) -> io::Error {
    io::Error::new(
        io::ErrorKind::Other,
        "named-pipe listener mutex was poisoned",
    )
}

pub(super) async fn connect(endpoint: &Endpoint) -> Result<RawConn, TransportError> {
    let pipe = endpoint.pipe_name();
    let mut attempts = 0u32;
    loop {
        match ClientOptions::new().open(pipe) {
            Ok(client) => return Ok(RawConn::Client(client)),
            // No pipe of this name: nothing is serving.
            Err(error) if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND) => {
                return Err(TransportError::NotRunning);
            }
            // Server up but no free instance: retry briefly, then report Busy.
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                attempts += 1;
                if attempts >= BUSY_RETRY_ATTEMPTS {
                    return Err(TransportError::Busy);
                }
                sleep(BUSY_RETRY_DELAY).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}
