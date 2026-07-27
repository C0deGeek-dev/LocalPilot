//! `localpilot agents …` — inspect the subagent definitions this project sees.
//!
//! Read-only. Listing or showing a definition never runs it: this surface exists
//! so a user can answer "which agents do I have, where did each come from, and
//! what is this one allowed to do" without starting a session.

use std::io::Write;
use std::path::Path;

use localpilot_agents::{AgentSet, DiscoveredAgent};

/// Resolve the definitions visible from `cwd`, including the per-user global
/// baseline unless `project_only`.
fn resolve(cwd: &Path, project_only: bool) -> AgentSet {
    let home = if project_only { None } else { home() };
    AgentSet::resolve(&AgentSet::standard_roots(cwd, home.as_deref()))
}

/// The user's home directory, when the platform reports one. Absent is normal
/// (some CI images), and only costs the global scopes.
pub fn home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
}

/// Print every effective agent, its origin, model, and tool count.
///
/// # Errors
/// Returns an error only if output cannot be written.
pub fn list(cwd: &Path, project_only: bool, out: &mut dyn Write) -> anyhow::Result<()> {
    let set = resolve(cwd, project_only);
    let agents = set.agents();

    if agents.is_empty() {
        writeln!(out, "No agent definitions found.")?;
        writeln!(
            out,
            "Add one at .localpilot/agents/<name>.agent.yaml (project) or \
             ~/.localpilot/agents/<name>.agent.yaml (all projects)."
        )?;
    } else {
        for agent in &agents {
            let d = &agent.definition;
            let model = d.model.as_deref().unwrap_or("(inherits session model)");
            let tools = if d.wants_all_tools() {
                "all parent tools".to_string()
            } else {
                format!("{} tool(s)", d.tools.len())
            };
            writeln!(
                out,
                "{name}  [{scope}]  {model}  {tools}\n    {description}",
                name = d.name,
                scope = agent.scope.label(),
                description = d.description.trim(),
            )?;
        }
    }

    report_shadowed_and_errors(&set, out)
}

/// Print one agent's resolved definition, including which prompt parts are on.
///
/// # Errors
/// Returns an error only if output cannot be written.
pub fn show(cwd: &Path, name: &str, project_only: bool, out: &mut dyn Write) -> anyhow::Result<()> {
    let set = resolve(cwd, project_only);
    let Some(agent) = set.get(name) else {
        writeln!(out, "No agent named {name:?}.")?;
        let known: Vec<&str> = set
            .agents()
            .iter()
            .map(|a| a.definition.name.as_str())
            .collect();
        if known.is_empty() {
            writeln!(out, "No definitions are visible from this directory.")?;
        } else {
            writeln!(out, "Known agents: {}", known.join(", "))?;
        }
        return report_shadowed_and_errors(&set, out);
    };

    print_agent(agent, out)?;
    report_shadowed_and_errors(&set, out)
}

fn print_agent(agent: &DiscoveredAgent, out: &mut dyn Write) -> anyhow::Result<()> {
    let d = &agent.definition;
    writeln!(out, "{} ({})", d.display(), d.name)?;
    writeln!(
        out,
        "  origin:      {} — {}",
        agent.scope.label(),
        agent.path.display()
    )?;
    writeln!(
        out,
        "  model:       {}",
        d.model.as_deref().unwrap_or("(inherits session model)")
    )?;
    if let Some(effort) = d.effort {
        writeln!(out, "  effort:      {}", effort.as_str())?;
    }
    writeln!(out, "  description: {}", d.description.trim())?;

    if d.wants_all_tools() {
        writeln!(
            out,
            "  tools:       * (everything the calling session holds)"
        )?;
    } else if d.tools.is_empty() {
        writeln!(out, "  tools:       none")?;
    } else {
        writeln!(out, "  tools:       {}", d.tools.join(", "))?;
    }

    let parts = d.prompt_parts;
    let on = |flag: bool| if flag { "on" } else { "off" };
    writeln!(
        out,
        "  prompt parts: base={} editing={} safety={} tools={} look-before-launch={}",
        on(parts.include_base),
        on(parts.include_editing_guidance),
        on(parts.include_safety),
        on(parts.include_tool_instructions),
        on(parts.include_look_before_launch),
    )?;
    if !parts.include_safety() {
        writeln!(
            out,
            "  note: safety framing is off — this agent's prompt omits the \
             permission-profile and commit-etiquette guidance."
        )?;
    }
    writeln!(out, "\n  prompt:")?;
    for line in d.prompt.lines() {
        writeln!(out, "    {line}")?;
    }
    Ok(())
}

/// Report shadowed definitions and load failures. Both are printed for every
/// command: a definition that silently does not load is the failure mode this
/// surface exists to prevent.
fn report_shadowed_and_errors(set: &AgentSet, out: &mut dyn Write) -> anyhow::Result<()> {
    if !set.shadowed().is_empty() {
        writeln!(
            out,
            "\nShadowed (a higher-precedence definition of the same name wins):"
        )?;
        for agent in set.shadowed() {
            writeln!(
                out,
                "  {} [{}] {}",
                agent.definition.name,
                agent.scope.label(),
                agent.path.display()
            )?;
        }
    }
    if !set.errors().is_empty() {
        writeln!(out, "\nCould not load:")?;
        for error in set.errors() {
            writeln!(out, "  {}: {}", error.path.display(), error.reason)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_agent(dir: &Path, file: &str, body: &str) {
        fs::create_dir_all(dir).expect("dir");
        fs::write(dir.join(file), body).expect("write");
    }

    fn project(tmp: &Path) -> std::path::PathBuf {
        let dir = tmp.join(".localpilot").join("agents");
        write_agent(
            &dir,
            "reviewer.agent.yaml",
            "format_version: 1\nname: reviewer\ndescription: Reviews a diff.\ntools:\n  - read_file\nprompt: Review it.\n",
        );
        tmp.to_path_buf()
    }

    #[test]
    fn list_reports_each_agent_with_its_origin() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = project(tmp.path());
        let mut out = Vec::new();
        list(&cwd, true, &mut out).expect("writes");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("reviewer"), "{text}");
        assert!(text.contains("project (.localpilot)"), "{text}");
        assert!(text.contains("1 tool(s)"), "{text}");
    }

    #[test]
    fn list_says_so_when_there_is_nothing_and_shows_where_to_put_one() {
        let tmp = tempfile::tempdir().expect("tmp");
        let mut out = Vec::new();
        list(tmp.path(), true, &mut out).expect("writes");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("No agent definitions found"), "{text}");
        assert!(text.contains(".localpilot/agents/"), "{text}");
    }

    #[test]
    fn show_prints_the_resolved_definition_and_its_prompt_parts() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = project(tmp.path());
        let mut out = Vec::new();
        show(&cwd, "reviewer", true, &mut out).expect("writes");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("prompt parts:"), "{text}");
        assert!(text.contains("safety=on"), "{text}");
        assert!(
            text.contains("Review it."),
            "the prompt body is shown: {text}"
        );
    }

    #[test]
    fn show_of_an_unknown_name_lists_what_is_known() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = project(tmp.path());
        let mut out = Vec::new();
        show(&cwd, "nope", true, &mut out).expect("writes");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("No agent named"), "{text}");
        assert!(text.contains("reviewer"), "known agents listed: {text}");
    }

    #[test]
    fn a_broken_definition_is_reported_rather_than_silently_missing() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = project(tmp.path());
        write_agent(
            &cwd.join(".localpilot").join("agents"),
            "broken.agent.yaml",
            "format_version: 1\nname: broken\n",
        );
        let mut out = Vec::new();
        list(&cwd, true, &mut out).expect("writes");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("Could not load"), "{text}");
        assert!(text.contains("broken.agent.yaml"), "{text}");
    }

    #[test]
    fn turning_safety_off_is_called_out_in_show() {
        let tmp = tempfile::tempdir().expect("tmp");
        let cwd = tmp.path().to_path_buf();
        write_agent(
            &cwd.join(".localpilot").join("agents"),
            "loose.agent.yaml",
            "format_version: 1\nname: loose\ndescription: d\nprompt_parts:\n  include_safety: false\nprompt: p\n",
        );
        let mut out = Vec::new();
        show(&cwd, "loose", true, &mut out).expect("writes");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("safety=off"), "{text}");
        assert!(text.contains("note: safety framing is off"), "{text}");
    }
}
