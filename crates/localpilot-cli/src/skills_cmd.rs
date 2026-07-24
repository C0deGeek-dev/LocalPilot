//! `localpilot skills`: the deterministic, user-facing skill surface.
//!
//! `list` shows the effective skills — the user-global baseline
//! (`~/.localpilot/skills`, `~/.agents/skills`) overlaid by the project skills,
//! one per name with the project definition winning a collision (LocalHub#39).
//! `show <name>` prints one skill's body by exact name — a deterministic load
//! with no model in the loop. Both are read-only and expose each skill's origin.
//! This is the user side of the skill model (ADR-0027); the model-callable
//! `skill_search`/`skill_load` tools are the pull-based counterpart and are off
//! by default.

use std::io::Write;
use std::path::Path;

use localpilot_core::{one_line, SUMMARY_CHARS};
use localpilot_skills::{discover_trusted, Invocation, SkillSet};

/// List the effective skills (global baseline overlaid by the project) with
/// their invocation, origin scope, and a one-line summary. The user explicitly
/// invoked this, so the project overlay is loaded (the workspace is trusted).
///
/// # Errors
/// Returns an error only if output cannot be written.
pub fn list(root: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    match discover_trusted(root, true) {
        Ok(set) => render_list(&set, out),
        Err(err) => {
            writeln!(out, "could not read skills: {err}")?;
            Ok(())
        }
    }
}

/// Render an effective skill set as a list (the testable core of [`list`]).
///
/// # Errors
/// Returns an error only if output cannot be written.
fn render_list(set: &SkillSet, out: &mut dyn Write) -> anyhow::Result<()> {
    // A malformed skill is skipped, not fatal — warn about it but still list the
    // valid ones (LocalHub#38).
    for warning in set.skipped() {
        writeln!(out, "warning: skipped a malformed skill — {warning}")?;
    }
    let names = set.names();
    if names.is_empty() {
        writeln!(
            out,
            "no skills found (looked under ~/.localpilot/skills, ~/.agents/skills, \
             .localpilot/skills, and .agents/skills)"
        )?;
        return Ok(());
    }
    writeln!(out, "skills:")?;
    for name in names {
        if let Some(skill) = set.by_name(name) {
            let invocation = match skill.manifest.invocation {
                Invocation::UserOnly => "user-only",
                Invocation::Discoverable => "discoverable",
            };
            writeln!(
                out,
                "- {name} [{invocation}, {}]: {}",
                skill.scope.label(),
                one_line(&skill.manifest.description, SUMMARY_CHARS)
            )?;
        }
    }
    writeln!(out, "\nRead one with: localpilot skills show <name>")?;
    Ok(())
}

/// Print one skill's body by exact name (a deterministic load). An unknown name is
/// a clean message, never an error.
///
/// # Errors
/// Returns an error only if output cannot be written.
pub fn show(root: &Path, name: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    match discover_trusted(root, true) {
        Ok(set) => render_show(&set, name, out),
        Err(err) => {
            writeln!(out, "could not read skills: {err}")?;
            Ok(())
        }
    }
}

/// Render one effective skill's body by name (the testable core of [`show`]).
///
/// # Errors
/// Returns an error only if output cannot be written.
fn render_show(set: &SkillSet, name: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    for warning in set.skipped() {
        writeln!(out, "warning: skipped a malformed skill — {warning}")?;
    }
    match set.by_name(name.trim()) {
        Some(skill) => {
            writeln!(
                out,
                "# skill: {} [{}]",
                skill.manifest.name,
                skill.scope.label()
            )?;
            if let Some(hint) = &skill.manifest.argument_hint {
                writeln!(out, "argument: {hint}")?;
            }
            if !skill.manifest.required_tools.is_empty() {
                writeln!(
                    out,
                    "declares required tools: {}",
                    skill.manifest.required_tools.join(", ")
                )?;
            }
            if !skill.manifest.permissions.is_empty() {
                writeln!(
                    out,
                    "declares permissions: {} (not granted; any action still goes through the \
                     permission gate)",
                    skill.manifest.permissions.join(", ")
                )?;
            }
            writeln!(out, "\n{}", skill.instructions.trim_end())?;
        }
        None => writeln!(out, "no skill named \"{}\"", name.trim())?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// Write a `SKILL.md`-only skill under `<root>/<sub>/<name>`, where `sub` is
    /// e.g. `.localpilot/skills` or `.agents/skills`. `root` is a project root or
    /// an injected home directory.
    fn write_skill_md_in(root: &Path, sub: &str, name: &str, description: &str, user_only: bool) {
        let mut dir = root.to_path_buf();
        for part in sub.split('/') {
            dir.push(part);
        }
        dir.push(name);
        std::fs::create_dir_all(&dir).unwrap();
        let flag = if user_only {
            "disable-model-invocation: true\n"
        } else {
            ""
        };
        std::fs::write(
            dir.join("SKILL.md"),
            format!(
                "---\nname: {name}\ndescription: {description}\n{flag}---\n\nBody of {name}.\n"
            ),
        )
        .unwrap();
    }

    /// A project `.localpilot/skills` skill.
    fn write_skill_md(root: &Path, name: &str, description: &str, user_only: bool) {
        write_skill_md_in(root, ".localpilot/skills", name, description, user_only);
    }

    /// The effective set for a project, with the global baseline injected from
    /// `home` (or `None`), so tests never touch the host's real home.
    fn resolve(root: &Path, home: Option<&Path>) -> SkillSet {
        localpilot_skills::discover(root, home, true).unwrap()
    }

    #[test]
    fn list_shows_invocation_origin_and_summary() {
        let dir = tempfile::tempdir().unwrap();
        write_skill_md(dir.path(), "add-provider", "guide adding a provider", false);
        write_skill_md(dir.path(), "secret-step", "by hand only", true);
        let mut buf = Vec::new();
        render_list(&resolve(dir.path(), None), &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains("add-provider [discoverable, project (.localpilot)]"),
            "{text}"
        );
        assert!(
            text.contains("secret-step [user-only, project (.localpilot)]"),
            "{text}"
        );
    }

    #[test]
    fn list_includes_global_skills_and_marks_project_overrides() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        // A global-only skill, and a name defined in both scopes.
        write_skill_md_in(
            home.path(),
            ".agents/skills",
            "threejs-webgl",
            "global three.js",
            false,
        );
        write_skill_md_in(
            home.path(),
            ".agents/skills",
            "modern-web-design",
            "global design",
            false,
        );
        write_skill_md(project.path(), "modern-web-design", "project design", false);

        let mut buf = Vec::new();
        render_list(&resolve(project.path(), Some(home.path())), &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        // Global-only skill shows its global origin…
        assert!(
            text.contains("threejs-webgl [discoverable, global (.agents)]"),
            "{text}"
        );
        // …and the overridden name appears once, as the project definition.
        assert!(
            text.contains("modern-web-design [discoverable, project (.localpilot)]"),
            "{text}"
        );
        assert_eq!(
            text.matches("modern-web-design").count(),
            1,
            "duplicate name in listing: {text}"
        );
    }

    #[test]
    fn list_summary_is_capped_to_one_line_with_ellipsis() {
        // Equivalence guard for the move to localpilot_core::one_line: a long skill
        // description is collapsed to one capped line + ellipsis in the user listing.
        let dir = tempfile::tempdir().unwrap();
        let long = format!("guide adding {}", "a provider integration ".repeat(20));
        write_skill_md(dir.path(), "add-provider", long.trim(), false);
        let mut buf = Vec::new();
        render_list(&resolve(dir.path(), None), &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        let line = text
            .lines()
            .find(|l| l.contains("add-provider"))
            .expect("listing line");
        assert!(line.contains('…'), "summary not ellipsized: {line:?}");
        assert!(
            line.chars().count() < long.chars().count(),
            "summary not truncated: {line:?}"
        );
    }

    #[test]
    fn show_prints_a_body_with_origin_and_a_clean_miss_otherwise() {
        let dir = tempfile::tempdir().unwrap();
        write_skill_md(dir.path(), "add-provider", "guide adding a provider", false);

        let mut hit = Vec::new();
        render_show(&resolve(dir.path(), None), "add-provider", &mut hit).unwrap();
        let text = String::from_utf8(hit).unwrap();
        assert!(text.contains("Body of add-provider"), "{text}");
        assert!(
            text.contains("project (.localpilot)"),
            "origin not shown: {text}"
        );

        let mut miss = Vec::new();
        render_show(&resolve(dir.path(), None), "nope", &mut miss).unwrap();
        assert!(String::from_utf8(miss).unwrap().contains("no skill named"));
    }

    #[test]
    fn show_reaches_a_global_skill_from_an_unrelated_project() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_skill_md_in(
            home.path(),
            ".localpilot/skills",
            "threejs-webgl",
            "global",
            false,
        );

        let mut buf = Vec::new();
        render_show(
            &resolve(project.path(), Some(home.path())),
            "threejs-webgl",
            &mut buf,
        )
        .unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("Body of threejs-webgl"), "{text}");
        assert!(
            text.contains("global (.localpilot)"),
            "origin not shown: {text}"
        );
    }
}
