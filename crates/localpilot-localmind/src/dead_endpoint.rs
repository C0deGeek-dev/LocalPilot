//! A local endpoint that is reliably unreachable, for tests that need a
//! configured-but-broken inference server.
//!
//! The obvious way to get one is to bind an ephemeral port, read its number, and
//! drop the listener — "the port is now closed, so connecting will be refused".
//! That is a race, and it is invisible until the suite is busy: between the drop
//! and the connect, **the operating system is free to hand that exact port to
//! anyone else asking for an ephemeral one**, which under `cargo test
//! --workspace` is several other tests doing the same thing. When it happens the
//! "dead" endpoint is alive, the request that was supposed to fail succeeds
//! against a stranger's listener, and the test fails somewhere far from the
//! cause.
//!
//! So the port is *held* for as long as the test needs it, and unreachability
//! comes from the server's behaviour rather than from its absence: a thread
//! accepts each connection and immediately closes it. The client sees the
//! connection die before a response — an error, which is what the test wants —
//! and no other process can take the port while the guard is alive.

use std::io::Write;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// An address that answers every request by hanging up.
///
/// Keep the guard alive for as long as the address must stay unusable; dropping
/// it releases the port and stops the thread.
pub(crate) struct DeadEndpoint {
    addr: SocketAddr,
    stopping: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl DeadEndpoint {
    /// Claim an ephemeral port and start refusing on it.
    ///
    /// # Panics
    /// If no local port can be bound, which means the test could not run anyway.
    pub(crate) fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local ephemeral port");
        let addr = listener.local_addr().expect("read the bound address");
        let stopping = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stopping);

        let thread = std::thread::spawn(move || {
            for incoming in listener.incoming() {
                if flag.load(Ordering::Relaxed) {
                    return;
                }
                // Close without answering. A client mid-request sees the
                // connection end, which is the failure the test is about.
                if let Ok(stream) = incoming {
                    let _ = stream.shutdown(Shutdown::Both);
                }
            }
        });

        Self {
            addr,
            stopping,
            thread: Some(thread),
        }
    }

    /// The base URL to configure, e.g. `http://127.0.0.1:53124`.
    pub(crate) fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for DeadEndpoint {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        // `accept` is blocking, so the flag alone would leave the thread parked
        // forever. One throwaway connection wakes it to observe the flag.
        if let Ok(mut stream) = TcpStream::connect(self.addr) {
            let _ = stream.write_all(b"");
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn the_port_stays_claimed_while_the_guard_lives() {
        let dead = DeadEndpoint::new();
        // The whole point: nobody else can take this port, so it cannot come
        // back to life underneath a test.
        assert!(
            TcpListener::bind(dead.addr).is_err(),
            "the guard must own the port, or the race it exists to close is still open"
        );
        assert!(dead.url().starts_with("http://127.0.0.1:"));
    }

    #[test]
    fn a_request_to_it_fails_rather_than_being_answered() {
        let dead = DeadEndpoint::new();
        let mut stream = TcpStream::connect(dead.addr).expect("it accepts, then hangs up");
        let _ = stream.write_all(b"POST /embed HTTP/1.1\r\nHost: x\r\n\r\n");
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);
        assert!(
            response.is_empty(),
            "an endpoint that answered would not be dead: {response:?}"
        );
    }

    #[test]
    fn the_port_is_released_when_the_guard_is_dropped() {
        let addr = {
            let dead = DeadEndpoint::new();
            dead.addr
        };
        assert!(
            TcpListener::bind(addr).is_ok(),
            "dropping the guard must free the port and stop the thread"
        );
    }
}
