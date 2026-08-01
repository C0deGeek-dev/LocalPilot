//! Opt-in local-IPC transport and daemon lifecycle for a LocalPilot server.
//!
//! This crate provides the groundwork for an opt-in, single-machine server that
//! a future `serve`/`connect` command will drive: a cross-platform framed local
//! transport, the daemon lifecycle around it, and a registry that hosts many
//! [`SessionRuntime`](localpilot_harness)s in one process. A tiny echo/ping
//! serve loop ([`wire::serve_echo`]) proves the transport end to end.
//!
//! The pieces:
//!
//! - [`transport`] — a uniform [`Listener`]/[`Conn`]/[`connect`] surface over a
//!   Unix domain socket (Unix) or a named pipe (Windows), with a deterministic
//!   per-workspace [`Endpoint`] path scheme. Records are framed with
//!   `localpilot-rpc`'s LF-delimited NDJSON codec, reused as-is.
//! - [`daemon`] — detached spawn of the current executable, a retry-connect
//!   ready handshake, and single-owner exclusivity with stale-endpoint reaping.
//! - [`registry`] — a process-local map of live sessions, each owned by one
//!   task and driven through a per-session async lock, with a caller-supplied
//!   factory that keeps session construction out of this crate.
//! - [`host`] — a per-session layer over one registry handle giving multi-client
//!   event fanout (a session-lifetime broadcast) and lock-free out-of-band
//!   cancel/steer that reach an in-flight turn without taking the runtime mutex
//!   the turn holds.
//! - [`attach`] — the connection-scoped bind seam: one decoded
//!   [`AttachTarget`](localpilot_rpc::AttachTarget) (open-new / resume-by-id /
//!   resume-by-name) routed to the registry, returning the bound session id
//!   (or a typed [`AttachError`] on an unknown id/name — never a panic).
//!
//! Every lifecycle primitive is built from safe `std` + `tokio` only: no
//! `unsafe`, no `libc`/`nix`, no `flock`/`setsid`/`kill`. Exclusivity uses an
//! atomic exclusive-create lock file (Unix) or the first-pipe-instance flag
//! (Windows); detachment uses `process_group(0)` (Unix) or `DETACHED_PROCESS |
//! CREATE_NO_WINDOW` (Windows) with null stdio.

#![forbid(unsafe_code)]

pub mod attach;
pub mod daemon;
pub mod host;
pub mod registry;
pub mod transport;
pub mod wire;

pub use attach::{attach, AttachError};
pub use daemon::{
    acquire, build_serve_command, ensure_running, spawn_detached, spawn_detached_argv,
    wait_for_ready, Acquired, DaemonError, Singleton, SERVE_ARGV,
};
pub use host::{Control, ControlOutcome, HostStatus, SessionHost};
pub use registry::{RegistryError, SessionFactory, SessionHandle, SessionRegistry};
pub use transport::{connect, Conn, Endpoint, Listener, TransportError};
pub use wire::serve_echo;
