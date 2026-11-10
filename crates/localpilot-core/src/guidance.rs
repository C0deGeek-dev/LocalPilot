//! Where a piece of task guidance came from.
//!
//! Guidance is text that tells the model how to do the work: a project's
//! instruction file, an installed skill package, a learned skill. The code that
//! produces such text is the only place that knows which file or package it
//! came from and why it was chosen, so it reports that here, as a typed value
//! beside the text. Nothing downstream parses the text to find out.
//!
//! A [`GuidanceSource`] describes; it grants nothing. Reporting a source never
//! changes what is sent to a model, which tools exist, or what is permitted.

/// What kind of thing the guidance is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GuidanceKind {
    /// A project instruction file (`AGENTS.md`, a path-scoped rule, ...).
    ProjectInstructions,
    /// An installed `SKILL.md` package.
    PackageSkill,
    /// A human-enabled skill distilled from accepted memory.
    LearnedSkill,
    /// A reviewed run recipe from a project package.
    ProjectRecipe,
    /// A fixed sentence the host itself adds about guidance (for example that
    /// packages are installed while discovery is off).
    HostNotice,
}

/// How much of the guidance the text carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GuidanceForm {
    /// A name and a summary: enough to choose it, not to follow it.
    Metadata,
    /// The instruction text itself.
    Body,
}

/// One piece of guidance: what it is, where it lives, and why it was chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuidanceSource {
    pub kind: GuidanceKind,
    pub form: GuidanceForm,
    /// A stable name within the kind: a skill name, an instruction file path.
    pub id: String,
    /// Where it came from, for a reader: a scope, a tier, a version.
    pub origin: String,
    /// Why it is here, in the words of the code that chose it.
    pub reason: String,
}

impl GuidanceSource {
    /// A source of `kind` and `form` named `id`.
    #[must_use]
    pub fn new(
        kind: GuidanceKind,
        form: GuidanceForm,
        id: impl Into<String>,
        origin: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            form,
            id: id.into(),
            origin: origin.into(),
            reason: reason.into(),
        }
    }
}
