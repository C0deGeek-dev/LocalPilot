//! Opt-in local-IPC transport and daemon lifecycle for a LocalPilot server.
//!
//! This crate provides the groundwork for an opt-in, single-machine server that
//! a future `serve`/`connect` command will drive: a cross-platform framed local
//! transport and the daemon lifecycle around it. It deliberately does **not**
//! host a [`SessionRuntime`](localpilot_harness) yet — session hosting is a
//! later subject. A tiny echo/ping serve loop ([`wire::serve_echo`]) proves the
//! transport end to end.
//!
//! Two things live here:
//!
//! - [`transport`] — a uniform [`Listener`]/[`Conn`]/[`connect`] surface over a
//!   Unix domain socket (Unix) or a named pipe (Windows), with a deterministic
//!   per-workspace [`Endpoint`] path scheme. Records are framed with
//!   `localpilot-rpc`'s LF-delimited NDJSON codec, reused as-is.
//! - [`daemon`] — detached spawn of the current executable, a retry-connect
//!   ready handshake, and single-owner exclusivity with stale-endpoint reaping.
//!
//! Every lifecycle primitive is built from safe `std` + `tokio` only: no
//! `unsafe`, no `libc`/`nix`, no `flock`/`setsid`/`kill`. Exclusivity uses an
//! atomic exclusive-create lock file (Unix) or the first-pipe-instance flag
//! (Windows); detachment uses `process_group(0)` (Unix) or `DETACHED_PROCESS |
//! CREATE_NO_WINDOW` (Windows) with null stdio.

#![forbid(unsafe_code)]

pub mod daemon;
pub mod transport;
pub mod wire;

pub use daemon::{
    acquire, build_serve_command, ensure_running, spawn_detached, spawn_detached_argv,
    wait_for_ready, Acquired, DaemonError, Singleton, SERVE_ARGV,
};
pub use transport::{connect, Conn, Endpoint, Listener, TransportError};
pub use wire::serve_echo;
