//! Model-callable, read-only tools that make project-local skills a live,
//! pull-based surface (ADR-0027).
//!
//! Discovery is pull-based, not pushed: `skill_search` returns lean ranked
//! locators over the *discoverable* skills, and `skill_load` returns one skill's
//! body by exact name. Both are read-only (`Effect::ReadPath`) — loading a skill
//! injects *content the agent reads*, never an action. A skill's declared
//! permissions/required tools are surfaced when it is loaded, but loading grants
//! nothing: any real effect the guidance leads to still goes through the
//! permission engine (no side channel). Project-local skills load only when the
//! workspace is trusted.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use localpilot_core::{one_line, word_overlap, Locator, SUMMARY_CHARS};
use localpilot_sandbox::Effect;
use localpilot_tools::{Tool, ToolContext, ToolError, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::error::SkillError;
use crate::loader::{discovery_roots, global_only_roots, home_dir, Skill, SkillSet};

/// Locators returned by a search are capped so a turn spends a bounded number of
/// tokens to *find* a skill before paying for any body.
const MAX_LOCATORS: usize = 10;
/// Upper bound on a single loaded skill body, so pulling guidance stays lean.
const BODY_CHARS: usize = 12_000;

/// Resolve the effective skill set for `root`, resolving the per-user home
/// directory from the environment. The per-user global baseline
/// (`~/.localpilot/skills`, `~/.agents/skills`) is always included; the
/// project overlay is **gated on workspace trust** — an untrusted workspace
/// contributes no project skills and so cannot shadow a global skill
/// (LocalHub#39).
///
/// # Errors
/// Returns [`SkillError`] if a discovered manifest or frontmatter fails to parse.
pub fn discover_trusted(root: &Path, trusted: bool) -> Result<SkillSet, SkillError> {
    discover(root, home_dir().as_deref(), trusted)
}

/// Resolve the effective skill set for `root` against an explicit `home` (the
/// per-user global baseline root, or `None` to omit the global layer). The
/// injectable seam behind [`discover_trusted`]: the global baseline is always
/// included, the project overlay only when `trusted`.
///
/// # Errors
/// Returns [`SkillError`] if a discovered manifest or frontmatter fails to parse.
pub fn discover(root: &Path, home: Option<&Path>, trusted: bool) -> Result<SkillSet, SkillError> {
    SkillSet::resolve(&discovery_roots(root, home, trusted))
}

/// Resolve the effective skill set for `root`, optionally restricted to the
/// user-global scope (`global_only`). The global baseline is resolved from the
/// environment home; when `global_only` is false the trusted project overlay is
/// added. Backs `skills list [-g]` / `skills show [-g]` (LocalHub#40).
///
/// # Errors
/// Returns [`SkillError`] if a discovered manifest or frontmatter fails to parse.
pub fn discover_trusted_scoped(
    root: &Path,
    trusted: bool,
    global_only: bool,
) -> Result<SkillSet, SkillError> {
    let home = home_dir();
    if global_only {
        SkillSet::resolve(&global_only_roots(home.as_deref()))
    } else {
        discover(root, home.as_deref(), trusted)
    }
}

/// The per-user home directory used for the global skill scope, resolved
/// cross-platform. Exposed so a caller (the CLI) can construct a
/// [`crate::SkillsManager`] with the same home the discovery layer uses. `None`
/// when no home is set.
#[must_use]
pub fn user_home() -> Option<PathBuf> {
    home_dir()
}

/// A simple relevance score for a locator: how many of the query's words the
/// skill's description contains, plus a bonus for an explicit command trigger
/// match. Mirrors the predicate [`SkillSet::relevant`] uses, so a skill that is
/// returned always scores at least 1.
fn score(skill: &Skill, query_words: &[&str], query_lower: &str) -> u32 {
    let description = skill.manifest.description.to_ascii_lowercase();
    let word_hits = word_overlap(&description, query_words);
    let trigger_bonus = u32::from(
        skill
            .manifest
            .triggers
            .commands
            .iter()
            .any(|c| query_lower.contains(&c.to_ascii_lowercase())),
    ) * 2;
    word_hits + trigger_bonus
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SkillSearchInput {
    /// What the agent is trying to do; matched against discoverable skills'
    /// descriptions and triggers.
    query: String,
}

/// `skill_search`: find skills relevant to a query, returning lean ranked
/// locators (name + one-line summary + score) over the *discoverable* skills only.
/// Searches the effective merged catalog — the user-global baseline plus the
/// trusted project overlay (LocalHub#39). Read-only; loads no bodies and surfaces
/// no user-only skill.
pub struct SkillSearch {
    /// The per-user home directory for the global skill baseline, resolved once
    /// at construction. `None` omits the global layer (e.g. no resolvable home).
    home: Option<PathBuf>,
}

impl SkillSearch {
    /// Construct the tool, resolving the per-user home directory from the
    /// environment for the global skill baseline.
    #[must_use]
    pub fn new() -> Self {
        Self { home: home_dir() }
    }
}

impl Default for SkillSearch {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for SkillSearch {
    fn name(&self) -> &str {
        "skill_search"
    }

    fn description(&self) -> &str {
        "Search this project's skills for ones relevant to the current task, returning a short \
         ranked list of locators (skill name, one-line summary, score) — no skill bodies. Skills \
         are advisory prompt modules; this is the pull-based way to discover them on demand instead \
         of carrying every skill in context. Then call `skill_load` with a name to read one skill's \
         guidance. Read-only: searching never runs, installs, or enables anything."
    }

    fn schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(SkillSearchInput)).unwrap_or(Value::Null)
    }

    fn approval_detail(&self, input: &Value) -> String {
        input
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(160)
            .collect()
    }

    fn effects(&self, _input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        Ok(vec![Effect::ReadPath {
            inside_workspace: true,
            secret_like: false,
        }])
    }

    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: SkillSearchInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        // The user-global baseline is always available; the project overlay is
        // included only when the workspace is trusted (LocalHub#39).
        let set = match discover(ctx.workspace.root(), self.home.as_deref(), ctx.trusted) {
            Ok(set) => set,
            Err(_) => return Ok(ToolOutput::ok("skills are unreadable")),
        };

        let query_lower = input.query.to_ascii_lowercase();
        let query_words: Vec<&str> = query_lower
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| w.len() > 2)
            .collect();
        let mut locators: Vec<Locator> = set
            .relevant(&input.query)
            .into_iter()
            .map(|skill| {
                Locator::new(
                    skill.manifest.name.clone(),
                    one_line(&skill.manifest.description, SUMMARY_CHARS),
                    score(skill, &query_words, &query_lower),
                )
            })
            .collect();
        // Highest score first; ties broken by name for a stable order.
        locators.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.name.cmp(&b.name)));
        locators.truncate(MAX_LOCATORS);

        if locators.is_empty() {
            return Ok(ToolOutput::ok(format!(
                "no skills match \"{}\"",
                input.query
            )));
        }
        let mut out = String::from(
            "Matching skills (locators only — call `skill_load` with a name to read one):\n",
        );
        for loc in &locators {
            let _ = writeln!(out, "- {} (score {}): {}", loc.name, loc.score, loc.summary);
        }
        Ok(ToolOutput::ok(out))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SkillLoadInput {
    /// The exact name of the skill to read (from `skill_search`, or a name the
    /// user typed).
    name: String,
}

/// `skill_load`: read one skill's body by exact name from the effective merged
/// catalog — the user-global baseline plus the trusted project overlay
/// (LocalHub#39). Works for any skill by name (the deterministic load path); the
/// body is advisory guidance the agent applies in its own reasoning. The skill's
/// declared required tools/permissions are surfaced, but loading grants nothing.
pub struct SkillLoad {
    /// The per-user home directory for the global skill baseline, resolved once
    /// at construction. `None` omits the global layer (e.g. no resolvable home).
    home: Option<PathBuf>,
}

impl SkillLoad {
    /// Construct the tool, resolving the per-user home directory from the
    /// environment for the global skill baseline.
    #[must_use]
    pub fn new() -> Self {
        Self { home: home_dir() }
    }
}

impl Default for SkillLoad {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for SkillLoad {
    fn name(&self) -> &str {
        "skill_load"
    }

    fn description(&self) -> &str {
        "Read one project skill's body by its exact name (from `skill_search`, or a name the user \
         asked for). The body is advisory guidance to apply in your own reasoning — loading it runs, \
         installs, and enables nothing. Any required tools or permissions the skill names are shown \
         for transparency; they are not granted, so any real action still goes through the normal \
         permission gate."
    }

    fn schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(SkillLoadInput)).unwrap_or(Value::Null)
    }

    fn approval_detail(&self, input: &Value) -> String {
        input
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(160)
            .collect()
    }

    fn effects(&self, _input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        // Loading a skill is a read inside the workspace and nothing more — never a
        // permission side channel, whatever the skill declares.
        Ok(vec![Effect::ReadPath {
            inside_workspace: true,
            secret_like: false,
        }])
    }

    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: SkillLoadInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        // The user-global baseline is always available; the project overlay is
        // included only when the workspace is trusted (LocalHub#39).
        let set = match discover(ctx.workspace.root(), self.home.as_deref(), ctx.trusted) {
            Ok(set) => set,
            Err(_) => return Ok(ToolOutput::ok("skills are unreadable")),
        };
        match set.by_name(input.name.trim()) {
            Some(skill) => Ok(ToolOutput::ok(render_skill(skill))),
            None => Ok(ToolOutput::ok(format!(
                "no skill named \"{}\"",
                input.name.trim()
            ))),
        }
    }
}

/// Render a loaded skill as advisory guidance: a header that surfaces its declared
/// required tools/permissions (transparency, not a grant), then the bounded body.
fn render_skill(skill: &Skill) -> String {
    let mut out = format!(
        "Skill `{}` [{}] (advisory guidance — apply it yourself; loading runs nothing):\n",
        skill.manifest.name,
        skill.scope.label()
    );
    if let Some(hint) = &skill.manifest.argument_hint {
        let _ = writeln!(out, "argument: {hint}");
    }
    if !skill.manifest.required_tools.is_empty() {
        let _ = writeln!(
            out,
            "declares required tools: {}",
            skill.manifest.required_tools.join(", ")
        );
    }
    if !skill.manifest.permissions.is_empty() {
        let _ = writeln!(
            out,
            "declares permissions: {} — not granted by loading; any action still goes through the \
             permission gate",
            skill.manifest.permissions.join(", ")
        );
    }
    out.push('\n');
    let body: String = skill.instructions.chars().take(BODY_CHARS).collect();
    out.push_str(&body);
    if skill.instructions.chars().count() > BODY_CHARS {
        out.push_str("\n…(truncated)");
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use localpilot_sandbox::{Interactivity, Workspace};
    use serde_json::json;
    use std::path::Path;

    fn write_skill_md(root: &Path, name: &str, description: &str, user_only: bool, extra: &str) {
        let dir = root.join(".localpilot").join("skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let flag = if user_only {
            "disable-model-invocation: true\n"
        } else {
            ""
        };
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n{flag}---\n\n{extra}Body of {name}.\n"),
        )
        .unwrap();
    }

    fn ctx(ws: &Workspace, trusted: bool) -> ToolContext<'_> {
        ToolContext {
            workspace: ws,
            interactivity: Interactivity::NonInteractive,
            trusted,
            retention: None,
            processes: None,
        }
    }

    /// A `skill_search` tool with an injected (or absent) global-baseline home,
    /// so tests never depend on the host's real home directory.
    fn search(home: Option<&Path>) -> SkillSearch {
        SkillSearch {
            home: home.map(Path::to_path_buf),
        }
    }

    /// A `skill_load` tool with an injected (or absent) global-baseline home.
    fn load(home: Option<&Path>) -> SkillLoad {
        SkillLoad {
            home: home.map(Path::to_path_buf),
        }
    }

    #[test]
    fn discover_gates_the_project_overlay_but_never_the_global_baseline() {
        let dir = tempfile::tempdir().unwrap();
        write_skill_md(
            dir.path(),
            "add-provider",
            "guide adding a provider",
            false,
            "",
        );

        // Untrusted, no home: project-local skills are not loaded, and there is
        // no global layer to fall back to.
        let untrusted = discover(dir.path(), None, false).unwrap();
        assert!(untrusted.names().is_empty());

        // Trusted: the project skill is discovered.
        let trusted = discover(dir.path(), None, true).unwrap();
        assert_eq!(trusted.names(), vec!["add-provider"]);
    }

    #[tokio::test]
    async fn search_returns_discoverable_locators_without_bodies() {
        let dir = tempfile::tempdir().unwrap();
        write_skill_md(
            dir.path(),
            "add-provider",
            "guide adding a provider",
            false,
            "",
        );
        write_skill_md(
            dir.path(),
            "secret-step",
            "guide adding a provider by hand",
            true,
            "",
        );
        let ws = Workspace::new(dir.path()).unwrap();

        let out = search(None)
            .invoke(
                json!({ "query": "how do I guide adding a provider" }),
                &ctx(&ws, true),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        // The discoverable skill is listed; the user-only one never is.
        assert!(out.text.contains("add-provider"), "got: {}", out.text);
        assert!(
            !out.text.contains("secret-step"),
            "user-only skill leaked: {}",
            out.text
        );
        // Locators only — no skill body text.
        assert!(
            !out.text.contains("Body of add-provider"),
            "body leaked into search: {}",
            out.text
        );
    }

    #[tokio::test]
    async fn search_locator_summary_is_capped_to_one_line_with_ellipsis() {
        // Equivalence guard for the move to localpilot_core::one_line: a long,
        // multi-word description must still collapse to a single capped summary
        // ending in an ellipsis, never dump the whole description into the locator.
        let dir = tempfile::tempdir().unwrap();
        let long = format!("guide adding {}", "a provider integration ".repeat(20));
        write_skill_md(dir.path(), "add-provider", long.trim(), false, "");
        let ws = Workspace::new(dir.path()).unwrap();

        let out = search(None)
            .invoke(json!({ "query": "guide adding provider" }), &ctx(&ws, true))
            .await
            .unwrap();
        let line = out
            .text
            .lines()
            .find(|l| l.contains("add-provider"))
            .expect("locator line");
        assert!(line.contains('…'), "summary not ellipsized: {line:?}");
        assert!(
            line.chars().count() < long.chars().count(),
            "summary was not truncated: {line:?}"
        );
    }

    #[tokio::test]
    async fn load_returns_a_body_for_a_known_name_and_a_clean_miss_otherwise() {
        let dir = tempfile::tempdir().unwrap();
        write_skill_md(
            dir.path(),
            "add-provider",
            "guide adding a provider",
            false,
            "",
        );
        let ws = Workspace::new(dir.path()).unwrap();

        let hit = load(None)
            .invoke(json!({ "name": "add-provider" }), &ctx(&ws, true))
            .await
            .unwrap();
        assert!(!hit.is_error);
        assert!(
            hit.text.contains("Body of add-provider"),
            "got: {}",
            hit.text
        );

        // An unknown name is a clean miss, not an error.
        let miss = load(None)
            .invoke(json!({ "name": "no-such-skill" }), &ctx(&ws, true))
            .await
            .unwrap();
        assert!(!miss.is_error);
        assert!(miss.text.contains("no skill named"), "got: {}", miss.text);
    }

    #[tokio::test]
    async fn load_surfaces_declared_permissions_but_grants_nothing() {
        let dir = tempfile::tempdir().unwrap();
        // A skill.toml that declares a write permission, plus its SKILL.md body.
        let sdir = dir.path().join(".localpilot").join("skills").join("writer");
        std::fs::create_dir_all(&sdir).unwrap();
        std::fs::write(
            sdir.join("skill.toml"),
            "name = \"writer\"\ndescription = \"writes files\"\nversion = \"0.1.0\"\npermissions = [\"write:repo\"]\n",
        )
        .unwrap();
        std::fs::write(sdir.join("SKILL.md"), "# writer\n\nDo the thing.\n").unwrap();
        let ws = Workspace::new(dir.path()).unwrap();

        let out = load(None)
            .invoke(json!({ "name": "writer" }), &ctx(&ws, true))
            .await
            .unwrap();
        // The declared permission is shown, framed as not-granted.
        assert!(
            out.text.contains("write:repo"),
            "permission not surfaced: {}",
            out.text
        );
        assert!(
            out.text.contains("not granted"),
            "no-grant framing missing: {}",
            out.text
        );

        // Loading a skill is a read inside the workspace and nothing more — no
        // permission side channel, whatever the skill declares.
        let effects = load(None)
            .effects(&json!({ "name": "writer" }), &ctx(&ws, true))
            .unwrap();
        assert_eq!(
            effects,
            vec![Effect::ReadPath {
                inside_workspace: true,
                secret_like: false
            }]
        );
    }

    #[tokio::test]
    async fn search_and_load_reach_a_global_skill_from_an_unrelated_project() {
        // A global skill under the injected home, and a project with none.
        let home = tempfile::tempdir().unwrap();
        write_skill_md(
            home.path(),
            "threejs-webgl",
            "guide building a three.js scene",
            false,
            "",
        );
        let project = tempfile::tempdir().unwrap();
        let ws = Workspace::new(project.path()).unwrap();

        // Search reaches the global skill…
        let found = search(Some(home.path()))
            .invoke(
                json!({ "query": "how do I build a three.js scene" }),
                &ctx(&ws, true),
            )
            .await
            .unwrap();
        assert!(found.text.contains("threejs-webgl"), "got: {}", found.text);

        // …and load returns its body, labelled as a global origin.
        let body = load(Some(home.path()))
            .invoke(json!({ "name": "threejs-webgl" }), &ctx(&ws, true))
            .await
            .unwrap();
        assert!(
            body.text.contains("Body of threejs-webgl"),
            "got: {}",
            body.text
        );
        assert!(
            body.text.contains("global"),
            "origin not shown: {}",
            body.text
        );
    }

    #[tokio::test]
    async fn untrusted_search_keeps_global_skills_but_drops_project_skills() {
        let home = tempfile::tempdir().unwrap();
        write_skill_md(
            home.path(),
            "global-helper",
            "guide a shared workflow",
            false,
            "",
        );
        let project = tempfile::tempdir().unwrap();
        write_skill_md(
            project.path(),
            "project-helper",
            "guide a shared workflow",
            false,
            "",
        );
        let ws = Workspace::new(project.path()).unwrap();

        // Untrusted: the global skill is still searchable; the project one is not.
        let out = search(Some(home.path()))
            .invoke(
                json!({ "query": "guide a shared workflow" }),
                &ctx(&ws, false),
            )
            .await
            .unwrap();
        assert!(
            out.text.contains("global-helper"),
            "global dropped: {}",
            out.text
        );
        assert!(
            !out.text.contains("project-helper"),
            "untrusted project skill leaked: {}",
            out.text
        );
    }

    #[tokio::test]
    async fn a_project_skill_shadows_a_global_skill_through_the_load_tool() {
        let home = tempfile::tempdir().unwrap();
        write_skill_md(
            home.path(),
            "modern-web-design",
            "the global one",
            false,
            "GLOBAL. ",
        );
        let project = tempfile::tempdir().unwrap();
        write_skill_md(
            project.path(),
            "modern-web-design",
            "the project one",
            false,
            "PROJECT. ",
        );
        let ws = Workspace::new(project.path()).unwrap();

        let out = load(Some(home.path()))
            .invoke(json!({ "name": "modern-web-design" }), &ctx(&ws, true))
            .await
            .unwrap();
        // The project package is effective, atomically — no global body leaks.
        assert!(out.text.contains("PROJECT."), "got: {}", out.text);
        assert!(
            !out.text.contains("GLOBAL."),
            "shadowed global leaked: {}",
            out.text
        );
        assert!(
            out.text.contains("project"),
            "origin not shown: {}",
            out.text
        );
    }
}
