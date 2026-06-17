//! `localpilot skills`: the deterministic, user-facing project-skill surface.
//!
//! `list` shows the discovered project skills; `show <name>` prints one skill's
//! body by exact name — a deterministic load with no model in the loop. Both are
//! read-only. This is the user side of the skill model (ADR-0027); the
//! model-callable `skill_search`/`skill_load` tools are the pull-based counterpart
//! and are off by default.

use std::io::Write;
use std::path::Path;

use localpilot_skills::{discover_trusted, Invocation};

/// List the project's discovered skills with their invocation and a one-line
/// summary. The user explicitly invoked this, so skills are loaded for the
/// current workspace.
///
/// # Errors
/// Returns an error only if output cannot be written.
pub fn list(root: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let set = match discover_trusted(root, true) {
        Ok(set) => set,
        Err(err) => {
            writeln!(out, "could not read project skills: {err}")?;
            return Ok(());
        }
    };
    let names = set.names();
    if names.is_empty() {
        writeln!(
            out,
            "no project skills found (looked under .localpilot/skills and .agents/skills)"
        )?;
        return Ok(());
    }
    writeln!(out, "project skills:")?;
    for name in names {
        if let Some(skill) = set.by_name(name) {
            let invocation = match skill.manifest.invocation {
                Invocation::UserOnly => "user-only",
                Invocation::Discoverable => "discoverable",
            };
            writeln!(
                out,
                "- {name} [{invocation}]: {}",
                one_line(&skill.manifest.description)
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
    let set = match discover_trusted(root, true) {
        Ok(set) => set,
        Err(err) => {
            writeln!(out, "could not read project skills: {err}")?;
            return Ok(());
        }
    };
    match set.by_name(name.trim()) {
        Some(skill) => {
            writeln!(out, "# skill: {}", skill.manifest.name)?;
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
        None => writeln!(out, "no project skill named \"{}\"", name.trim())?,
    }
    Ok(())
}

fn one_line(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut shown: String = collapsed.chars().take(100).collect();
    if collapsed.chars().count() > 100 {
        shown.push('…');
    }
    shown
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn write_skill_md(root: &Path, name: &str, description: &str, user_only: bool) {
        let dir = root.join(".localpilot").join("skills").join(name);
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

    #[test]
    fn list_shows_invocation_and_summary() {
        let dir = tempfile::tempdir().unwrap();
        write_skill_md(dir.path(), "add-provider", "guide adding a provider", false);
        write_skill_md(dir.path(), "secret-step", "by hand only", true);
        let mut buf = Vec::new();
        list(dir.path(), &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("add-provider [discoverable]"), "{text}");
        assert!(text.contains("secret-step [user-only]"), "{text}");
    }

    #[test]
    fn show_prints_a_body_for_a_known_name_and_a_clean_miss_otherwise() {
        let dir = tempfile::tempdir().unwrap();
        write_skill_md(dir.path(), "add-provider", "guide adding a provider", false);

        let mut hit = Vec::new();
        show(dir.path(), "add-provider", &mut hit).unwrap();
        assert!(String::from_utf8(hit)
            .unwrap()
            .contains("Body of add-provider"));

        let mut miss = Vec::new();
        show(dir.path(), "nope", &mut miss).unwrap();
        assert!(String::from_utf8(miss)
            .unwrap()
            .contains("no project skill named"));
    }
}
