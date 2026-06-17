//! Skill discovery and suggestion for LocalPilot.
//!
//! Owns skill discovery and loading, the skill manifest format, and skill
//! suggestion heuristics that generate disabled drafts from repeated workflows.
//! Auto-generated skills are suggestions until the user reviews and accepts them.
//! Skills declare the permissions their scripts/assets need; those declarations
//! are surfaced before execution and enforced by the permission engine — a skill
//! is never a permission side channel.
#![forbid(unsafe_code)]

mod error;
mod loader;
mod manifest;
mod suggest;
mod templates;

pub use error::SkillError;
pub use loader::{standard_skill_dirs, Skill, SkillSet};
pub use manifest::{Invocation, SkillManifest, SkillTriggers};
pub use suggest::{SkillDraft, SuggestionEngine};
pub use templates::{standard_template_dirs, PromptTemplate, TemplateSet};
