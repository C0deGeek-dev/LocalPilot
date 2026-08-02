//! A task graph: the plan several workers agree on, and the rules that keep them
//! from wrecking it.
//!
//! This crate is **pure**. It opens no file, holds no socket, spawns nothing,
//! and knows nothing about sessions, models, or tools. A [`TaskPlan`] is a value:
//! it is mutated by calling functions on it, serialised with `serde`, and — the
//! reason for all of the above — run to completion by a [`simulator`](sim) with
//! no live agents attached. Wiring it to real workers is somebody else's crate.
//!
//! That separation is the design. The hard part of running several agents on one
//! plan is not spawning them; it is that the graph is shared mutable state under
//! concurrent, unreliable, occasionally creative writers. Every rule that keeps
//! it coherent lives here, where it can be tested exhaustively in microseconds.
//!
//! # The shape
//!
//! ```text
//!   seed ──▶ [task] ──▶ [task] ──▶ [gate] ──▶ done
//!               │                     │
//!         expand│               inject│  (findings become new work,
//!               ▼                     ▼   and the gate re-reviews)
//!         [child] [child]        [remediation]
//! ```
//!
//! - A **task** waits on other tasks; readiness is derived from the edges, never
//!   stored ([`schedule::ready_nodes`]).
//! - A task may **expand** into children instead of doing the work. It does not
//!   disappear — it becomes a join over its children, so nothing downstream has
//!   to be rewired ([`ops::expand_node`]).
//! - A **gate** reviews everything it waits on, and raises findings by *adding
//!   work* rather than by complaining ([`ops::inject_from_gate`]). In
//!   [`PlanMode::Deep`] gates are inserted automatically over every seed and
//!   every expansion.
//! - Completing a task hands on a typed [`HandoffArtifact`] — findings,
//!   evidence, and, in deep mode, an explicit statement of what was *not*
//!   checked. Downstream tasks read it instead of redoing the work.
//!
//! # Example
//!
//! ```
//! use localpilot_taskgraph::{
//!     ops::seed, schedule::{dispatch, ready_nodes}, ActorId, NodeSpec, PlanMode, TaskPlan,
//! };
//!
//! let lead = ActorId::new("coordinator");
//! let mut plan = TaskPlan::new("make the tests pass", PlanMode::Light, lead.clone());
//! let seeded = seed(
//!     &mut plan,
//!     &lead,
//!     "seed-1",
//!     &[
//!         NodeSpec::task("find", "Find every failing test."),
//!         NodeSpec::task("fix", "Fix them.").after(0),
//!     ],
//! )?;
//!
//! // Only the root is ready; the fix waits on the survey.
//! assert_eq!(ready_nodes(&plan), vec![seeded.nodes[0]]);
//!
//! let assignment = dispatch(&mut plan, seeded.nodes[0], &ActorId::new("worker-1"))?;
//! assert!(assignment.input.contains("make the tests pass"));
//! # Ok::<(), localpilot_taskgraph::PlanError>(())
//! ```

#![forbid(unsafe_code)]

pub mod artifact;
pub mod error;
pub mod ops;
pub mod plan;
pub mod schedule;
pub mod sim;

pub use artifact::{parse_confidence, Confidence, ConfidenceError, HandoffArtifact};
pub use error::PlanError;
pub use plan::{ActorId, NodeId, NodeKind, NodeSpec, PlanMode, TaskNode, TaskPlan, TaskStatus};
pub use schedule::{Assignment, PlanProgress, Readiness};
pub use sim::{SimAction, SimConfig, SimEvent, SimExecutor, SimReport};
