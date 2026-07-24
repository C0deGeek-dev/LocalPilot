//! Skill discovery and suggestion for LocalPilot.
//!
//! Owns skill discovery and loading, the skill manifest format, and skill
//! suggestion heuristics that generate disabled drafts from repeated workflows.
//! Auto-generated skills are suggestions until the user reviews and accepts them.
//! Skills declare the permissions their scripts/assets need; those declarations
//! are surfaced before execution and enforced by the permission engine — a skill
//! is never a permission side channel.
#![forbid(unsafe_code)]

mod catalog;
mod error;
mod fetch;
mod install;
mod loader;
mod manager;
mod manifest;
mod source;
mod suggest;
mod templates;
mod tools;

pub use catalog::{read_catalog, Catalog, CatalogPackage, CATALOG_ROOTS};
pub use error::SkillError;
pub use fetch::{GitFetcher, RepoFetcher, Snapshot};
pub use install::{InstallLedger, Provenance};
pub use loader::{
    discovery_roots, global_skill_dirs, standard_skill_dirs, Skill, SkillScope, SkillSet,
};
pub use manager::{Approval, Confirm, InstallSpec, ReadScope, Scope, SkillsManager};
pub use manifest::{Invocation, SkillManifest, SkillTriggers};
pub use source::{normalize_url, source_id, SkillSource, SourceRegistry};
pub use suggest::{SkillDraft, SuggestionEngine};
pub use templates::{standard_template_dirs, PromptTemplate, TemplateSet};
pub use tools::{
    discover, discover_trusted, discover_trusted_scoped, user_home, SkillLoad, SkillSearch,
};
