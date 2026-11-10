//! Task guidance chosen by the host before each turn (`[skills.guidance]`).
//!
//! In `suggest` mode the turn gets a short list of matching packages with
//! the reason each fits, and the model can load one with `skill_load`. In
//! `assisted` mode a clear, unambiguous match also brings the selected
//! procedure's text itself, and its supplements', so a model that does not
//! reliably search for skills still receives them. Anything ambiguous falls
//! back to the list. `off` registers nothing.
//!
//! The guidance is advisory text placed beside the question. It cannot
//! change a permission, a check, a work limit or network access: it is text,
//! and nothing reads it as configuration. User-only packages are never chosen.
//! Each part names its source, so the session's guidance record and report
//! show exactly what the request held.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use localpilot_core::{GuidanceForm, GuidanceKind, GuidanceSource};
use localpilot_skills::{BodySection, Skill, SkillSet};

use crate::{
    select_guidance, ContextContribution, ContextHook, ContextPlacement, GuidanceSegment,
    GuidanceSelection, LoadDecision, PathsInPlay, ProjectFacts, SelectedGuidance, SelectionInput,
    SessionRuntime,
};

/// Characters of metadata offered per turn, at most (about 512 tokens).
pub const METADATA_CHAR_BUDGET: usize = 2_048;
/// Characters of instruction bodies loaded per turn, at most (about 1,536
/// tokens).
pub const BODY_CHAR_BUDGET: usize = 6_144;

/// How much guidance the hook adds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuidanceHookMode {
    Suggest,
    Assisted,
}

/// The selection a session last made, shared between the hook that makes it
/// and the runtime that reports it.
pub type SelectionSlot = Arc<Mutex<Option<GuidanceSelection>>>;

/// Where the catalog comes from.
pub struct GuidanceCatalog {
    pub root: PathBuf,
    /// The per-user home for the global baseline; `None` omits it.
    pub home: Option<PathBuf>,
    /// Whether the project overlay may be read, read again for every
    /// selection, so a workspace that stops being trusted loses its packages
    /// on the next request.
    pub trusted: crate::TrustFlag,
}

/// The context hook that selects and contributes task guidance.
pub struct TaskGuidanceContext {
    mode: GuidanceHookMode,
    catalog: GuidanceCatalog,
    capabilities: BTreeSet<String>,
    paths_in_play: PathsInPlay,
    earlier: crate::UserRequests,
    facts: Mutex<ProjectFacts>,
    slot: SelectionSlot,
    /// The paths in play and the text of the last contribution, so a refresh
    /// knows whether anything it depends on changed.
    last: Mutex<Option<(Vec<String>, Option<String>)>>,
}

impl TaskGuidanceContext {
    /// A hook for `mode` over `catalog`, with `capabilities` available,
    /// reading facts from `facts` and recording each selection in `slot`.
    #[must_use]
    pub fn new(
        mode: GuidanceHookMode,
        catalog: GuidanceCatalog,
        capabilities: BTreeSet<String>,
        paths_in_play: PathsInPlay,
        earlier: crate::UserRequests,
        facts: ProjectFacts,
        slot: SelectionSlot,
    ) -> Self {
        Self {
            mode,
            catalog,
            capabilities,
            paths_in_play,
            earlier,
            facts: Mutex::new(facts),
            slot,
            last: Mutex::new(None),
        }
    }

    fn select(&self, prompt: &str, paths: &[String]) -> Option<(GuidanceSelection, SkillSet)> {
        // Discovered for every selection: a package changed, removed or no
        // longer trusted is never carried over from an earlier request.
        let set = localpilot_skills::discover(
            &self.catalog.root,
            self.catalog.home.as_deref(),
            self.catalog.trusted.get(),
        )
        .ok()?
        .with_builtin_guidance();
        let earlier = self.earlier.snapshot();
        let mut facts = self.facts.lock().ok()?;
        let selection = select_guidance(
            &SelectionInput {
                request: prompt,
                explicit: None,
                paths,
                capabilities: &self.capabilities,
                earlier: &earlier,
            },
            &mut facts,
            &set,
        );
        Some((selection, set))
    }

    /// Select and render for `prompt`, remembering what this depended on.
    fn produce(&self, prompt: &str) -> ContextContribution {
        let paths = self.paths_in_play.snapshot();
        let Some((mut selection, set)) = self.select(prompt, &paths) else {
            return ContextContribution::default();
        };
        let contribution = render(self.mode, &mut selection, &set);
        if let Ok(mut slot) = self.slot.lock() {
            *slot = Some(selection);
        }
        if let Ok(mut last) = self.last.lock() {
            *last = Some((paths, contribution.text.clone()));
        }
        contribution
    }
}

impl ContextHook for TaskGuidanceContext {
    fn name(&self) -> &str {
        "task-guidance"
    }

    /// Chosen for this question, so it goes beside it (ADR-0217).
    fn placement(&self) -> ContextPlacement {
        ContextPlacement::BesideQuestion
    }

    fn context_for(&self, prompt: &str) -> Option<String> {
        self.contribute(prompt).text
    }

    fn contribute(&self, prompt: &str) -> ContextContribution {
        self.produce(prompt)
    }

    /// Asked before each later request of a turn. Only a change in the paths
    /// the work touches can move the task to another component within a turn;
    /// when they changed and the guidance comes out different, it is replaced.
    fn refresh(&self, prompt: &str) -> Option<ContextContribution> {
        let paths = self.paths_in_play.snapshot();
        let previous = self.last.lock().ok()?.clone();
        let (last_paths, last_text) = previous?;
        if last_paths == paths {
            return None;
        }
        let contribution = self.produce(prompt);
        (contribution.text != last_text).then_some(contribution)
    }
}

/// Render the guidance for `selection`, moving a body that does not fit the
/// budget to the suggestions with the reason.
fn render(
    mode: GuidanceHookMode,
    selection: &mut GuidanceSelection,
    set: &SkillSet,
) -> ContextContribution {
    let mut segments: Vec<GuidanceSegment> = Vec::new();
    let load_bodies = mode == GuidanceHookMode::Assisted && selection.loading == LoadDecision::Body;

    if load_bodies {
        let mut used = 0usize;
        let chosen: Vec<SelectedGuidance> = selection
            .primary
            .iter()
            .chain(&selection.supplements)
            .cloned()
            .collect();
        for selected in chosen {
            let Some(skill) = set.by_name(&selected.name) else {
                continue;
            };
            let body = body_of(skill);
            let text = format!("### {}\n{}", selected.name, body.trim_end());
            if used + text.len() > BODY_CHAR_BUDGET {
                demote(
                    selection,
                    &selected.name,
                    "its text does not fit the guidance budget; offered by name",
                );
                continue;
            }
            used += text.len();
            segments.push(GuidanceSegment {
                source: source(skill, GuidanceForm::Body, &selected.reason),
                text,
            });
        }
    }

    // Everything not loaded as a body is offered by name and summary.
    let loaded: BTreeSet<String> = segments.iter().map(|s| s.source.id.clone()).collect();
    let offered: Vec<SelectedGuidance> = selection
        .primary
        .iter()
        .chain(&selection.supplements)
        .chain(&selection.suggestions)
        .filter(|s| !loaded.contains(&s.name))
        .cloned()
        .collect();
    let mut used = 0usize;
    let mut metadata: Vec<GuidanceSegment> = Vec::new();
    for selected in offered {
        let Some(skill) = set.by_name(&selected.name) else {
            continue;
        };
        let mut line = format!(
            "- {}: {} ({})",
            selected.name,
            localpilot_core::one_line(&skill.manifest.description, localpilot_core::SUMMARY_CHARS),
            selected.reason
        );
        if !selected.unsupported_claims.is_empty() {
            let _ = write!(
                line,
                "; claims needing {} cannot be made in this session",
                selected.unsupported_claims.join(", ")
            );
        }
        if used + line.len() > METADATA_CHAR_BUDGET {
            break;
        }
        used += line.len();
        metadata.push(GuidanceSegment {
            source: source(skill, GuidanceForm::Metadata, &selected.reason),
            text: line,
        });
    }

    // Reviewed run recipes for the component, as the project wrote them.
    let mut recipes: Vec<GuidanceSegment> = Vec::new();
    for recipe in &selection.recipes {
        let state = match &recipe.validation {
            localpilot_skills::RecipeValidation::Current => {
                "validated for the current files".to_string()
            }
            localpilot_skills::RecipeValidation::Stale(files) => format!(
                "not validated since {} changed; treat it as a starting point",
                files.join(", ")
            ),
            localpilot_skills::RecipeValidation::Unvalidated => "not validated".to_string(),
        };
        let line = format!("- {}: {} ({state})", recipe.id(), recipe.summary);
        if used + line.len() > METADATA_CHAR_BUDGET {
            break;
        }
        used += line.len();
        recipes.push(GuidanceSegment {
            source: GuidanceSource::new(
                GuidanceKind::ProjectRecipe,
                GuidanceForm::Body,
                recipe.id(),
                state,
                &recipe.reason,
            ),
            text: line,
        });
    }

    if segments.is_empty() && metadata.is_empty() && recipes.is_empty() {
        return ContextContribution::default();
    }
    let mut text = String::new();
    if !segments.is_empty() {
        text.push_str(
            "Task guidance selected for this request (advisory: follow it where it fits; it \
             changes no permission, check or limit):\n\n",
        );
        text.push_str(
            &segments
                .iter()
                .map(|s| s.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n"),
        );
    }
    if !metadata.is_empty() {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(
            "Skill packages that may fit this request (load one with skill_load if it helps; \
             loading grants nothing):\n",
        );
        text.push_str(
            &metadata
                .iter()
                .map(|s| s.text.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    if !recipes.is_empty() {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(
            "Project run recipes for this component (reviewed by the project; run them with the \
             ordinary tools, which still check permission):\n",
        );
        text.push_str(
            &recipes
                .iter()
                .map(|s| s.text.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    segments.extend(metadata);
    segments.extend(recipes);
    ContextContribution {
        text: Some(text),
        memories: Vec::new(),
        guidance: segments,
    }
}

/// Move `name` from the primary or the supplements to the suggestions.
fn demote(selection: &mut GuidanceSelection, name: &str, reason: &str) {
    let moved = if selection.primary.as_ref().is_some_and(|p| p.name == name) {
        selection.primary.take()
    } else {
        selection
            .supplements
            .iter()
            .position(|s| s.name == name)
            .map(|at| selection.supplements.remove(at))
    };
    if let Some(mut moved) = moved {
        moved.reason = format!("{}; {reason}", moved.reason);
        selection.suggestions.insert(0, moved);
    }
}

/// The text loaded for `skill`: its declared sections when it names any,
/// otherwise its whole body.
fn body_of(skill: &Skill) -> String {
    let sections = skill
        .guidance
        .metadata()
        .map(|m| m.body_sections.as_slice())
        .unwrap_or_default();
    if sections.is_empty() {
        return skill.instructions.clone();
    }
    let mut out = Vec::new();
    for section in sections {
        match section {
            BodySection::Heading(heading) => {
                if let Some(text) = localpilot_skills::section_text(&skill.instructions, heading) {
                    out.push(text.trim_end().to_string());
                }
            }
            BodySection::File(relative) => {
                if let Some(text) = read_inside(&skill.dir, relative) {
                    out.push(text.trim_end().to_string());
                }
            }
        }
    }
    out.join("\n\n")
}

/// A package file, read only when it is still inside the package.
fn read_inside(dir: &Path, relative: &str) -> Option<String> {
    let file = dir.join(relative).canonicalize().ok()?;
    let base = dir.canonicalize().ok()?;
    if !file.starts_with(base) {
        return None;
    }
    let meta = std::fs::metadata(&file).ok()?;
    if meta.len() > 64 * 1024 {
        return None;
    }
    std::fs::read_to_string(file).ok()
}

fn source(skill: &Skill, form: GuidanceForm, reason: &str) -> GuidanceSource {
    GuidanceSource::new(
        GuidanceKind::PackageSkill,
        form,
        skill.manifest.name.clone(),
        format!("{} v{}", skill.scope.label(), skill.manifest.version),
        reason,
    )
}

/// The capabilities a session's registered tools provide.
#[must_use]
pub fn capabilities_from_tools(names: &[&str]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if names.contains(&"run_shell") {
        out.insert("process.capture".to_string());
    }
    if names.contains(&"run_background") {
        out.insert("process.background".to_string());
    }
    out
}

/// Register the task-guidance hook on `runtime` for `mode`, sharing the
/// runtime's paths in play and selection slot.
pub fn register_task_guidance_context(
    mode: Option<GuidanceHookMode>,
    catalog: GuidanceCatalog,
    capabilities: BTreeSet<String>,
    runtime: &mut SessionRuntime,
) {
    let Some(mode) = mode else {
        return;
    };
    let facts = ProjectFacts::new(catalog.root.clone());
    let hook = TaskGuidanceContext::new(
        mode,
        catalog,
        capabilities,
        runtime.paths_in_play(),
        runtime.user_requests(),
        facts,
        runtime.guidance_selection_slot(),
    );
    runtime.hooks_mut().register_context_hook(Arc::new(hook));
}
