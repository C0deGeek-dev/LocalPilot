//! Swarms: several sessions in one repository working on one plan.
//!
//! A swarm is opt-in and additive. Nothing here runs unless something asks for
//! it, and a session that never joins one behaves exactly as it did before.
//!
//! - [`scope`] decides *which* swarm a directory belongs to — the repository, not
//!   the path, so worktrees of one repo are one swarm.
//! - [`registry`] holds who is in it, who spawned whom, and the caps that keep
//!   fan-out bounded.
//! - [`spawn`] starts workers under those caps, runs them in parallel, and flows
//!   their answers back to whoever spawned them.
//! - [`messaging`] routes agent-to-agent messages: scope is the spawn tree, and
//!   delivery rides the same soft-interrupt substrate the user's own steering
//!   uses.
//! - [`driver`] runs a plan: dispatch what is ready, refill as workers finish,
//!   stop when nothing can move.
//! - [`converge`] is the sibling driver for a *symmetric pair*: two peers exchange
//!   proposals and revisions of one artifact until both agree or a bound stops
//!   them. Not a plan and not a hierarchy — a transport-agnostic bounded protocol,
//!   designed to run over the existing messaging substrate through an abstract
//!   endpoint boundary. A production adapter now supplies that boundary over real
//!   adopted-session hosts; no user-runnable pair entrypoint ships yet.
//! - [`lifecycle`] is what happens when a worker stops answering: heartbeats,
//!   salvage, re-election, reparenting, reaping, and durable snapshots.
//! - [`touches`] records who touched which file recently and tells the peers a
//!   change affects. Advisory only: nothing is locked and nothing is rolled
//!   back.

pub mod converge;
pub mod driver;
pub mod lifecycle;
pub mod messaging;
/// The real `PairEndpoints` adapter for an adopted pair. Private: it registers
/// the trait impl on `AdoptedPair`, which needs no name of its own.
mod pair_endpoints;
pub mod registry;
pub mod scope;
pub mod spawn;
pub mod touches;

pub use converge::{
    pair_session_directive, CandidateSnapshot, EndpointError, NotifyReply, PairAbort, PairBounds,
    PairDriver, PairEndpoints, PairOutcome, PairProgress, PairProgressRx, PairReport,
    PairSetupError, TurnReply,
};
pub use driver::{run_plan, DriverConfig, RunReport};
pub use lifecycle::{reap_terminal, salvage, sweep, Salvaged, SnapshotStore, SwarmSnapshot};
pub use messaging::SessionPeers;
pub use registry::{
    Admission, MemberRole, MemberStatus, Reservation, SwarmError, SwarmLimits, SwarmMember,
    SwarmRegistry,
};
pub use scope::{git_common_dir, swarm_id_for_dir, SwarmId, SWARM_ID_ENV};
pub use spawn::{
    AdoptedPair, SpawnError, SpawnRequest, Spawned, SwarmHost, WorkerFactory, WorkerReport,
};
pub use touches::{announce, Collision, TouchIndex};
