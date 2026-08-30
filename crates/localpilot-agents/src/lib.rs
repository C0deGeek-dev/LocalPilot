//! Declarative subagents: definitions, discovery, and grant resolution.
//!
//! A **subagent** is a bounded child session with its own context window, its
//! own prompt, and its own — always narrower — tool set. It is declared in a
//! YAML file rather than compiled in, so adding a specialised agent is a file,
//! not a release.
//!
//! This crate owns the *data* half: parsing and validating a definition,
//! discovering definitions with the same precedence users already know from
//! skills, and resolving a definition's tool list into the child's actual grants
//! by intersecting it with the parent's. It deliberately owns no execution —
//! running a child session needs the harness, which depends on this crate.
//!
//! **Subagents are not skills.** A skill is text the model may read; loading one
//! grants nothing. A subagent is an execution with authority, and that authority
//! is always a subset of the caller's. The two share no loader, no registry, and
//! no file format, so nothing can drift from "advisory prompt module" into
//! "thing that can run commands".
#![forbid(unsafe_code)]

mod definition;
mod error;
mod grants;
mod loader;
mod template;

pub use definition::{validate_name, AgentDefinition, Effort, PromptParts, FORMAT_VERSION};
pub use error::AgentError;
pub use grants::{resolve as resolve_grants, Grants};
pub use loader::{AgentLoadError, AgentScope, AgentSet, DiscoveredAgent};
pub use template::{render as render_prompt, validate as validate_prompt, Bindings, VOCABULARY};
