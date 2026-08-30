//! Model-callable, read-only tools that make project-local skills a live,
//! pull-based surface (ADR-0027).
//!
//! Discovery is pull-based, not pushed: `skill_list` pages the *discoverable*
//! catalog, `skill_search` returns lean ranked locators over it, and `skill_load`
//! returns one skill's body by exact name. All three are read-only
//! (`Effect::ReadPath`) — loading a skill
//! injects *content the agent reads*, never an action. A skill's declared
//! permissions/required tools are surfaced when it is loaded, but loading grants
//! nothing: any real effect the guidance leads to still goes through the
//! permission engine (no side channel). Project-local skills load only when the
//! workspace is trusted.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use localpilot_core::{one_line, Locator, SUMMARY_CHARS};
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
/// Default `skill_list` page size when the caller gives no `limit`.
const DEFAULT_LIST_LIMIT: usize = 50;
/// Hard ceiling on a `skill_list` page, so one call never returns an unbounded
/// catalog. A larger requested `limit` is capped to this; it never yields more.
const MAX_LIST_LIMIT: usize = 100;

/// The plural suffix for a count (`""` for 1, `"s"` otherwise).
fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

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
        "Search the installed SKILL.md package catalog (the user-global baseline plus this \
         workspace's trusted project overlay) for skills relevant to the current task, returning a \
         short ranked list of locators (skill name, one-line summary, score) — no skill bodies. \
         Skills are advisory prompt modules; this is the pull-based way to discover them on demand \
         instead of carrying every skill in context. Call `skill_list` to page the whole catalog, \
         or `skill_load` with an exact name to read one skill's guidance. Package skills only — \
         unrelated to LocalMind's active/draft skills (`active_skills`/`skill_drafts`). Read-only: \
         searching never runs, installs, or enables anything, and never surfaces a user-only skill."
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

        // One ranked pass over the whole matching set (score desc, name asc):
        // the count is taken before any truncation, so overflow is honest.
        let ranked = set.ranked(&input.query);
        let total = ranked.len();
        let mut locators: Vec<Locator> = ranked
            .into_iter()
            .map(|(skill, score)| {
                Locator::new(
                    skill.manifest.name.clone(),
                    one_line(&skill.manifest.description, SUMMARY_CHARS),
                    score,
                )
            })
            .collect();
        locators.truncate(MAX_LOCATORS);

        if locators.is_empty() {
            // Honest no-match: report how many discoverable packages exist (never
            // user-only), so the model learns skills are present rather than
            // concluding none exist. Lexical search may truthfully miss an
            // unrelated or non-English query; no full-catalog fallback.
            let available = set.discoverable_count();
            let plural = if available == 1 { "" } else { "s" };
            return Ok(ToolOutput::ok(format!(
                "no installed package skills strongly match \"{}\" — {available} discoverable \
                 package skill{plural} available; call skill_list or broaden the query",
                input.query
            )));
        }
        let mut out = String::from(
            "Matching skills (locators only — call `skill_load` with a name to read one):\n",
        );
        for loc in &locators {
            let _ = writeln!(out, "- {} (score {}): {}", loc.name, loc.score, loc.summary);
        }
        // Never silently drop matches beyond the page cap.
        if total > locators.len() {
            let _ = writeln!(
                out,
                "(showing {} of {total} matches; refine or call skill_list)",
                locators.len()
            );
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
        "Read one installed SKILL.md package's body by its exact name (from `skill_list` / \
         `skill_search`, or a name the user asked for) — the effective catalog is the user-global \
         baseline plus this workspace's trusted project overlay, and an exact name also reaches a \
         user-only package. The body is advisory guidance to apply in your own reasoning — loading \
         it runs, installs, and enables nothing. Any required tools or permissions the skill names \
         are shown for transparency; they are not granted, so any real action still goes through the \
         normal permission gate. Package skills only — unrelated to LocalMind's active/draft skills \
         (`active_skills`/`skill_drafts`)."
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

#[derive(Debug, Deserialize, JsonSchema)]
struct SkillListInput {
    /// 0-based index of the first skill to return (default 0).
    #[serde(default)]
    offset: usize,
    /// Maximum skills to return in this page (default 50; values above 100 are
    /// capped to 100; 0 is invalid).
    #[serde(default = "default_list_limit")]
    limit: usize,
}

fn default_list_limit() -> usize {
    DEFAULT_LIST_LIMIT
}

/// `skill_list`: page the installed SKILL.md package catalog — **discoverable**
/// skills only (name, one-line summary, origin scope), in stable name order.
/// Package skills only; LocalMind active/draft skills have their own tools. The
/// effective merged catalog (user-global baseline + trusted project overlay) is
/// used, so an untrusted workspace shows only the global baseline. Read-only;
/// loads no bodies and never reveals a user-only package's name, description, or
/// body.
pub struct SkillList {
    /// The per-user home directory for the global skill baseline, resolved once
    /// at construction. `None` omits the global layer (e.g. no resolvable home).
    home: Option<PathBuf>,
}

impl SkillList {
    /// Construct the tool, resolving the per-user home directory from the
    /// environment for the global skill baseline.
    #[must_use]
    pub fn new() -> Self {
        Self { home: home_dir() }
    }
}

impl Default for SkillList {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for SkillList {
    fn name(&self) -> &str {
        "skill_list"
    }

    fn description(&self) -> &str {
        "List the installed SKILL.md package catalog (the user-global baseline plus \
         this workspace's trusted project overlay) — discoverable skills only \
         (name, one-line summary, and origin scope), in name order and paginated. \
         Package skills only; for LocalMind active/draft skills use `active_skills` \
         or `skill_drafts`. Read-only: listing runs, installs, and enables nothing, \
         and never reveals a user-only package's name, description, or body."
    }

    fn schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(SkillListInput)).unwrap_or(Value::Null)
    }

    fn effects(&self, _input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        Ok(vec![Effect::ReadPath {
            inside_workspace: true,
            secret_like: false,
        }])
    }

    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: SkillListInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        // A zero page is a clean invalid input, never an empty looping page.
        if input.limit == 0 {
            return Err(ToolError::InvalidInput(
                "`limit` must be at least 1 (max 100)".to_string(),
            ));
        }
        let limit = input.limit.min(MAX_LIST_LIMIT);
        // The user-global baseline is always available; the project overlay is
        // included only when the workspace is trusted (LocalHub#39) — an untrusted
        // workspace never has its project manifests read here.
        let set = match discover(ctx.workspace.root(), self.home.as_deref(), ctx.trusted) {
            Ok(set) => set,
            Err(_) => return Ok(ToolOutput::ok("skills are unreadable")),
        };
        // Discoverable rows only, in the set's stable name order; user-only
        // packages contribute a count and nothing else.
        let skills: Vec<&Skill> = set.discoverable().collect();
        let total = skills.len();
        let user_only = set.user_only_count();

        if total == 0 {
            let mut out =
                String::from("No discoverable skill packages installed (0 discoverable).");
            if user_only > 0 {
                let _ = write!(
                    out,
                    " {user_only} user-only package{} installed but hidden from model discovery.",
                    plural(user_only)
                );
            }
            return Ok(ToolOutput::ok(out));
        }
        // An offset at or past the end is distinct from an empty catalog.
        if input.offset >= total {
            return Ok(ToolOutput::ok(format!(
                "offset {} is past the end — {total} discoverable package skill{} total; use offset 0",
                input.offset,
                plural(total)
            )));
        }

        let end = (input.offset + limit).min(total);
        let mut out = format!(
            "Installed skill packages — {total} discoverable, showing {}-{end} of {total} (name order):\n",
            input.offset + 1
        );
        for skill in &skills[input.offset..end] {
            let _ = writeln!(
                out,
                "- {} — {} [{}]",
                skill.manifest.name,
                one_line(&skill.manifest.description, SUMMARY_CHARS),
                skill.scope.label()
            );
        }
        if end < total {
            let _ = writeln!(out, "next offset: {end}");
        }
        if user_only > 0 {
            let _ = writeln!(
                out,
                "({user_only} user-only package{} installed but hidden from model discovery)",
                plural(user_only)
            );
        }
        Ok(ToolOutput::ok(out))
    }
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
            agents: None,
            prompter: None,
            peers: None,
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

    /// A `skill_list` tool with an injected (or absent) global-baseline home.
    fn list(home: Option<&Path>) -> SkillList {
        SkillList {
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
        assert!(!out.is_error());
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
    async fn no_match_reports_the_discoverable_count_never_absence_or_user_only() {
        let dir = tempfile::tempdir().unwrap();
        write_skill_md(
            dir.path(),
            "add-provider",
            "guide adding a provider",
            false,
            "",
        );
        // A user-only skill must not be counted or named.
        write_skill_md(dir.path(), "secret-step", "internal handoff", true, "");
        let ws = Workspace::new(dir.path()).unwrap();

        let out = search(None)
            .invoke(
                json!({ "query": "quantum teleportation recipe" }),
                &ctx(&ws, true),
            )
            .await
            .unwrap();
        assert!(!out.is_error());
        // Honest no-match: reports the discoverable count (1 — the user-only skill
        // is excluded), never "no skills exist".
        assert!(
            out.text
                .contains("no installed package skills strongly match"),
            "got: {}",
            out.text
        );
        assert!(
            out.text.contains("1 discoverable package skill available"),
            "got: {}",
            out.text
        );
        // Points the model at the list tool now that it exists.
        assert!(out.text.contains("skill_list"), "got: {}", out.text);
        assert!(
            !out.text.contains("secret-step"),
            "user-only skill leaked: {}",
            out.text
        );
    }

    #[tokio::test]
    async fn more_than_ten_matches_report_the_full_count_and_never_silently_truncate() {
        let dir = tempfile::tempdir().unwrap();
        // Twelve discoverable skills that all match the token "format".
        for i in 0..12 {
            write_skill_md(
                dir.path(),
                &format!("formatter-{i:02}"),
                "format helper",
                false,
                "",
            );
        }
        let ws = Workspace::new(dir.path()).unwrap();

        let out = search(None)
            .invoke(json!({ "query": "format" }), &ctx(&ws, true))
            .await
            .unwrap();
        assert!(!out.is_error());
        // The overflow is disclosed against the full matching set, not hidden.
        assert!(
            out.text.contains("showing 10 of 12 matches"),
            "got: {}",
            out.text
        );
        // Exactly ten locator lines are shown (the cap), the rest disclosed.
        let shown = out.text.lines().filter(|l| l.starts_with("- ")).count();
        assert_eq!(shown, 10);
        // The page is the STABLE top: all matches tie on score, so the name
        // tie-break yields formatter-00..=formatter-09; the last two are omitted.
        // This proves rank-before-truncate, not merely the line count.
        for i in 0..10 {
            assert!(
                out.text.contains(&format!("formatter-{i:02}")),
                "expected formatter-{i:02} in page: {}",
                out.text
            );
        }
        assert!(
            !out.text.contains("formatter-10"),
            "formatter-10 must be past the page: {}",
            out.text
        );
        assert!(
            !out.text.contains("formatter-11"),
            "formatter-11 must be past the page: {}",
            out.text
        );
        // The overflow disclosure names the list tool.
        assert!(out.text.contains("skill_list"), "got: {}", out.text);
    }

    // --- skill_list (LocalHub#60) ---

    /// Write `n` discoverable skills named `pkg-00`..`pkg-(n-1)` with a matching
    /// one-line description, under the project overlay.
    fn write_n_skills(root: &Path, n: usize) {
        for i in 0..n {
            write_skill_md(
                root,
                &format!("pkg-{i:02}"),
                &format!("helper number {i}"),
                false,
                "",
            );
        }
    }

    #[tokio::test]
    async fn skill_list_default_page_lists_the_whole_small_catalog_in_name_order() {
        let dir = tempfile::tempdir().unwrap();
        write_n_skills(dir.path(), 15);
        let ws = Workspace::new(dir.path()).unwrap();

        let out = list(None).invoke(json!({}), &ctx(&ws, true)).await.unwrap();
        assert!(!out.is_error());
        assert!(
            out.text.contains("15 discoverable, showing 1-15 of 15"),
            "got: {}",
            out.text
        );
        // All 15 present, one row each, with the scope label.
        let rows: Vec<&str> = out.text.lines().filter(|l| l.starts_with("- ")).collect();
        assert_eq!(rows.len(), 15);
        assert!(rows[0].contains("pkg-00"), "got: {}", rows[0]);
        assert!(
            rows[0].contains("[project (.localpilot)]"),
            "got: {}",
            rows[0]
        );
        // Name order is stable.
        assert!(rows[14].contains("pkg-14"), "got: {}", rows[14]);
        // Fits one page: no next offset.
        assert!(!out.text.contains("next offset"), "got: {}", out.text);
    }

    #[tokio::test]
    async fn skill_list_pages_with_next_offset() {
        let dir = tempfile::tempdir().unwrap();
        write_n_skills(dir.path(), 15);
        let ws = Workspace::new(dir.path()).unwrap();

        let out = list(None)
            .invoke(json!({ "offset": 0, "limit": 10 }), &ctx(&ws, true))
            .await
            .unwrap();
        assert!(out.text.contains("showing 1-10 of 15"), "got: {}", out.text);
        assert!(out.text.contains("next offset: 10"), "got: {}", out.text);
        let rows = out.text.lines().filter(|l| l.starts_with("- ")).count();
        assert_eq!(rows, 10);

        // The next page returns the remainder and no further offset.
        let out2 = list(None)
            .invoke(json!({ "offset": 10, "limit": 10 }), &ctx(&ws, true))
            .await
            .unwrap();
        assert!(
            out2.text.contains("showing 11-15 of 15"),
            "got: {}",
            out2.text
        );
        assert!(!out2.text.contains("next offset"), "got: {}", out2.text);
        assert_eq!(out2.text.lines().filter(|l| l.starts_with("- ")).count(), 5);
    }

    #[tokio::test]
    async fn skill_list_caps_the_limit_at_one_hundred_and_rejects_a_zero_limit() {
        let dir = tempfile::tempdir().unwrap();
        write_n_skills(dir.path(), 120);
        let ws = Workspace::new(dir.path()).unwrap();

        // A limit above the hard maximum never returns more than 100 rows.
        let out = list(None)
            .invoke(json!({ "limit": 1000 }), &ctx(&ws, true))
            .await
            .unwrap();
        assert!(
            out.text.contains("showing 1-100 of 120"),
            "got: {}",
            out.text
        );
        assert!(out.text.contains("next offset: 100"), "got: {}", out.text);
        assert_eq!(
            out.text.lines().filter(|l| l.starts_with("- ")).count(),
            100
        );

        // A zero limit is a clean invalid input, not an empty looping page.
        let err = list(None)
            .invoke(json!({ "limit": 0 }), &ctx(&ws, true))
            .await;
        assert!(
            matches!(err, Err(ToolError::InvalidInput(_))),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn skill_list_distinguishes_empty_catalog_from_out_of_range_offset() {
        // Empty catalog.
        let empty = tempfile::tempdir().unwrap();
        let ws_empty = Workspace::new(empty.path()).unwrap();
        let out = list(None)
            .invoke(json!({}), &ctx(&ws_empty, true))
            .await
            .unwrap();
        assert!(
            out.text
                .contains("No discoverable skill packages installed (0 discoverable)"),
            "got: {}",
            out.text
        );

        // Non-empty catalog, offset past the end (including offset == total).
        let dir = tempfile::tempdir().unwrap();
        write_n_skills(dir.path(), 3);
        let ws = Workspace::new(dir.path()).unwrap();
        let out = list(None)
            .invoke(json!({ "offset": 3 }), &ctx(&ws, true))
            .await
            .unwrap();
        assert!(
            out.text
                .contains("offset 3 is past the end — 3 discoverable package skills total"),
            "got: {}",
            out.text
        );
        // Distinct from the empty message.
        assert!(!out.text.contains("0 discoverable"), "got: {}", out.text);
    }

    #[tokio::test]
    async fn skill_list_shows_only_discoverable_and_reports_user_only_as_a_count() {
        let dir = tempfile::tempdir().unwrap();
        write_skill_md(
            dir.path(),
            "visible-pkg",
            "a discoverable helper",
            false,
            "",
        );
        // A user-only skill with a sentinel name and description that must not leak.
        write_skill_md(
            dir.path(),
            "hidden-sentinel",
            "SENTINEL-DESC-should-never-appear",
            true,
            "",
        );
        let ws = Workspace::new(dir.path()).unwrap();

        let out = list(None).invoke(json!({}), &ctx(&ws, true)).await.unwrap();
        assert!(out.text.contains("1 discoverable"), "got: {}", out.text);
        assert!(out.text.contains("visible-pkg"), "got: {}", out.text);
        // The user-only count is reported, but never its name or description.
        assert!(
            out.text
                .contains("1 user-only package installed but hidden"),
            "got: {}",
            out.text
        );
        assert!(
            !out.text.contains("hidden-sentinel"),
            "name leaked: {}",
            out.text
        );
        assert!(
            !out.text.contains("SENTINEL-DESC"),
            "description leaked: {}",
            out.text
        );
    }

    #[tokio::test]
    async fn skill_list_untrusted_shows_the_global_baseline_only_and_counts_only_its_user_only() {
        // Global baseline (injected home): one discoverable + one user-only.
        let home = tempfile::tempdir().unwrap();
        write_skill_md(home.path(), "global-visible", "a global helper", false, "");
        write_skill_md(home.path(), "global-hidden", "GLOBAL-SENTINEL", true, "");
        // Project overlay: its own discoverable + user-only, which must be neither
        // shown nor counted while untrusted.
        let project = tempfile::tempdir().unwrap();
        write_skill_md(
            project.path(),
            "project-visible",
            "a project helper",
            false,
            "",
        );
        write_skill_md(
            project.path(),
            "project-hidden",
            "PROJECT-SENTINEL",
            true,
            "",
        );
        let ws = Workspace::new(project.path()).unwrap();

        let out = list(Some(home.path()))
            .invoke(json!({}), &ctx(&ws, false))
            .await
            .unwrap();
        // The effective untrusted catalog is exactly the global baseline.
        assert!(out.text.contains("1 discoverable"), "got: {}", out.text);
        assert!(out.text.contains("global-visible"), "got: {}", out.text);
        // The omitted user-only count is exactly the GLOBAL one.
        assert!(
            out.text
                .contains("1 user-only package installed but hidden"),
            "got: {}",
            out.text
        );
        // No project contribution — row, name, description, or count.
        assert!(!out.text.contains("project-visible"), "got: {}", out.text);
        assert!(!out.text.contains("project-hidden"), "got: {}", out.text);
        assert!(!out.text.contains("PROJECT-SENTINEL"), "got: {}", out.text);
        // The global user-only description also never leaks.
        assert!(!out.text.contains("GLOBAL-SENTINEL"), "got: {}", out.text);
    }

    #[tokio::test]
    async fn skill_load_returns_a_user_only_package_body_by_exact_name() {
        let dir = tempfile::tempdir().unwrap();
        // A user-only package: excluded from list/search, reachable only by name.
        write_skill_md(dir.path(), "secret-runbook", "SECRET-DESC", true, "");
        write_skill_md(
            dir.path(),
            "visible-helper",
            "a discoverable helper",
            false,
            "",
        );
        let ws = Workspace::new(dir.path()).unwrap();

        // Exact user-supplied name loads its body through the tool.
        let loaded = load(None)
            .invoke(json!({ "name": "secret-runbook" }), &ctx(&ws, true))
            .await
            .unwrap();
        assert!(!loaded.is_error());
        assert!(
            loaded.text.contains("Body of secret-runbook"),
            "got: {}",
            loaded.text
        );

        // Neither list nor search ever surfaces that user-only name or description.
        let listed = list(None).invoke(json!({}), &ctx(&ws, true)).await.unwrap();
        assert!(
            !listed.text.contains("secret-runbook"),
            "got: {}",
            listed.text
        );
        assert!(!listed.text.contains("SECRET-DESC"), "got: {}", listed.text);
        let searched = search(None)
            .invoke(json!({ "query": "secret runbook" }), &ctx(&ws, true))
            .await
            .unwrap();
        assert!(
            !searched.text.contains("secret-runbook"),
            "got: {}",
            searched.text
        );
        assert!(
            !searched.text.contains("SECRET-DESC"),
            "got: {}",
            searched.text
        );
    }

    #[tokio::test]
    async fn skill_list_bounds_each_row_summary_to_one_line() {
        let dir = tempfile::tempdir().unwrap();
        // A long description with internal whitespace runs (a single frontmatter
        // line; the fixture writer does not support a raw multiline value). The
        // row must collapse the whitespace and bound the summary.
        let long = format!("verbose package{}", "  detail".repeat(40));
        write_skill_md(dir.path(), "verbose-pkg", &long, false, "");
        let ws = Workspace::new(dir.path()).unwrap();

        let out = list(None).invoke(json!({}), &ctx(&ws, true)).await.unwrap();
        // Exactly one locator row for the package.
        let rows: Vec<&str> = out.text.lines().filter(|l| l.starts_with("- ")).collect();
        assert_eq!(rows.len(), 1);
        // The summary is bounded (ellipsis) and does not carry the full tail.
        assert!(rows[0].contains('…'), "got: {}", rows[0]);
        assert!(
            !rows[0].contains("detail  detail"),
            "uncollapsed/unbounded description leaked into the row: {}",
            rows[0]
        );
    }

    #[test]
    fn skill_list_declares_one_in_workspace_read_effect() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let effects = list(None).effects(&json!({}), &ctx(&ws, true)).unwrap();
        assert_eq!(effects.len(), 1);
        assert!(
            matches!(
                effects[0],
                Effect::ReadPath {
                    inside_workspace: true,
                    secret_like: false
                }
            ),
            "got: {:?}",
            effects[0]
        );
    }

    #[test]
    fn the_three_package_tool_descriptions_name_the_catalog_and_cross_reference_localmind() {
        let list_tool = list(None);
        let search_tool = search(None);
        let load_tool = load(None);
        for (name, desc) in [
            ("skill_list", list_tool.description()),
            ("skill_search", search_tool.description()),
            ("skill_load", load_tool.description()),
        ] {
            assert!(
                desc.contains("SKILL.md"),
                "{name} must name the installed SKILL.md package catalog: {desc}"
            );
            // The effective-catalog origin/trust fact (global baseline + trusted overlay).
            assert!(
                desc.contains("user-global baseline"),
                "{name} must name the user-global baseline: {desc}"
            );
            assert!(
                desc.contains("trusted project overlay"),
                "{name} must name the trusted project overlay: {desc}"
            );
            // Both LocalMind cross-references.
            assert!(
                desc.contains("active_skills") && desc.contains("skill_drafts"),
                "{name} must cross-reference both LocalMind tools: {desc}"
            );
        }
        // skill_load still documents the exact user-supplied-name path.
        assert!(
            load_tool.description().contains("exact name"),
            "skill_load must name the exact-name path: {}",
            load_tool.description()
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
        assert!(!hit.is_error());
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
        assert!(!miss.is_error());
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
