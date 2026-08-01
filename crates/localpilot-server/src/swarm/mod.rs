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

pub mod registry;
pub mod scope;
pub mod spawn;

pub use registry::{
    Admission, MemberRole, MemberStatus, Reservation, SwarmError, SwarmLimits, SwarmMember,
    SwarmRegistry,
};
pub use scope::{git_common_dir, swarm_id_for_dir, SwarmId, SWARM_ID_ENV};
pub use spawn::{SpawnError, SpawnRequest, Spawned, SwarmHost, WorkerFactory, WorkerReport};
