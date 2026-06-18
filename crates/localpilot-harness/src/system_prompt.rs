//! Agent-mode system prompt.
//!
//! The prompt is first-party text for this project. It describes observable
//! runtime contracts and the currently registered tool names; provider-specific
//! adapters still supply the formal JSON schemas.

use localpilot_tools::ToolRegistry;

/// Build the agent-mode system prompt for the active tool registry.
///
/// `marker_enabled` adds the `NEED:` marker convention (ADR-0031) when the
/// pull-discovery broker's marker trigger is on; it is gated together with the
/// `tool_search` tool being registered.
#[must_use]
pub fn agent_system_prompt(tools: &ToolRegistry, marker_enabled: bool) -> String {
    let mut names = tools.names();
    names.sort_unstable();
    build_prompt_with(&names, marker_enabled)
}

/// The cue, appended only when a knowledge-base search tool is registered, that
/// tells the model to pull project facts on demand rather than assume they were
/// preloaded into context.
const KNOWLEDGE_SEARCH_CUE: &str = concat!(
    "\n\n",
    "The project has a searchable knowledge base. When a task depends on project ",
    "facts you have not already read — conventions, where something lives, prior ",
    "decisions — call `knowledge_search` to pull relevant indexed knowledge on ",
    "demand. It is not preloaded into the conversation, so search it instead of ",
    "guessing.",
);

/// The cue, appended only when the `remember` tool is registered, that tells the
/// model it can propose a durable lesson for human review as it works.
const REMEMBER_CUE: &str = concat!(
    "\n\n",
    "When you learn something durable about this project — a convention, a pitfall, ",
    "a decision worth keeping — call `remember` to propose it for human review. It ",
    "enqueues a review candidate; it never writes accepted memory directly. Use it ",
    "sparingly, not for transient notes.",
);

/// The cue, appended only when the `skill_drafts` tool is registered, that tells
/// the model candidate skill drafts may exist and that surfacing one never
/// activates it.
const SKILL_DRAFTS_CUE: &str = concat!(
    "\n\n",
    "This project may have generated skill drafts — candidate reusable workflows ",
    "distilled from accepted memory. When a task resembles a recurring workflow, call ",
    "`skill_drafts` to list or inspect them. They are always disabled; you can surface a ",
    "relevant one and propose it to the user, but enabling a skill stays a human step — ",
    "never assume a draft is active.",
);

/// The cue, appended only when the `skill_search` tool is registered (autonomous
/// skill discovery is enabled), that tells the model project skills are reachable
/// on demand by search rather than carried in context.
const SKILL_SEARCH_CUE: &str = concat!(
    "\n\n",
    "This project may define skills — advisory prompt modules for recurring tasks. They are not ",
    "loaded into context; when a task looks like one, call `skill_search` to find relevant skills ",
    "(you get back names and one-line summaries), then `skill_load` to read one and apply its ",
    "guidance yourself. Loading a skill runs nothing; any action it suggests still goes through the ",
    "normal permission gate.",
);

/// The cue, appended only when the `tool_search` tool is registered (the
/// pull-discovery broker is enabled), that tells the model the advertised tool set
/// is a working subset and the rest are reachable on demand by search.
const TOOL_SEARCH_CUE: &str = concat!(
    "\n\n",
    "The tools listed above are a working subset, not every tool available. When you need a ",
    "capability you do not see advertised, call `tool_search` to find the right tool (you get back ",
    "names and one-line summaries), then `tool_load` with a name to reveal its schema and call it. ",
    "If you call a tool that is not currently advertised, the system resolves it to the closest ",
    "available tool, reveals it, and asks you to retry. Revealing a tool only changes what is ",
    "advertised — it runs nothing and grants nothing, so any action still goes through the normal ",
    "permission gate.",
);

/// The marker nudge, appended only when the marker trigger is enabled *and* the
/// broker's `tool_search` is registered: teaches the model it can name a
/// capability it lacks on a line of its own so the harness reveals a tool
/// proactively (ADR-0031). Off by default — the marker needs new model behaviour.
const TOOL_MARKER_CUE: &str = concat!(
    "\n\n",
    "If you realize you need a capability you do not have advertised, you may write a line ",
    "`NEED: <capability>` (for example `NEED: fetch a web page`) and stop; the system will reveal ",
    "the closest available tool so you can call it on your next turn. This is optional — you can ",
    "also just call `tool_search` directly.",
);

/// Render the prompt from the sorted tool names with the marker nudge off. A
/// test-only convenience over [`build_prompt_with`] so the existing cue tests stay
/// terse; production code calls [`agent_system_prompt`].
#[cfg(test)]
fn build_prompt(names: &[&str]) -> String {
    build_prompt_with(names, false)
}

/// Render the prompt, optionally adding the `NEED:` marker convention.
fn build_prompt_with(names: &[&str], marker_enabled: bool) -> String {
    let knowledge_cue = if names.contains(&"knowledge_search") {
        KNOWLEDGE_SEARCH_CUE
    } else {
        ""
    };
    let remember_cue = if names.contains(&"remember") {
        REMEMBER_CUE
    } else {
        ""
    };
    let skill_drafts_cue = if names.contains(&"skill_drafts") {
        SKILL_DRAFTS_CUE
    } else {
        ""
    };
    let skill_search_cue = if names.contains(&"skill_search") {
        SKILL_SEARCH_CUE
    } else {
        ""
    };
    let tool_search_cue = if names.contains(&"tool_search") {
        TOOL_SEARCH_CUE
    } else {
        ""
    };
    // The marker convention only makes sense when the broker can act on it, so it
    // is gated on both the flag and `tool_search` being registered.
    let tool_marker_cue = if marker_enabled && names.contains(&"tool_search") {
        TOOL_MARKER_CUE
    } else {
        ""
    };
    format!(
        "\
You are LocalPilot's coding agent running in agent mode.

Work inside the current workspace. Read relevant files before changing them,
prefer precise edits over broad rewrites, and verify changes with the smallest
useful command before you finish. To change an existing file, default to
`replace_in_file` (replace an exact block of old text with new text — it may
span multiple lines); use `apply_patch` for changes across several files or
that create and delete files. Reserve `write_file` for brand-new files or a
full rewrite — do not use it to make a small edit. Respect the permission
profile: reads, writes, commands, and network effects may be denied or require
approval.

Even when running under `bypass` (which grants technical allow-all on commands
and file effects), do not commit or push changes unless the user explicitly asks
for it — `bypass` lifts the permission gate, but does not imply permission to
mutate history or share work without being told to.

Use tools when local information or side effects are needed. Available tools:
{tools}.{knowledge_cue}{remember_cue}{skill_drafts_cue}{skill_search_cue}{tool_search_cue}{tool_marker_cue}

Look before you launch. If a task names an existing target you can reach — a URL,
a running service, a `host:port` — inspect or probe it first (for example fetch or
curl it) before assuming you must create or launch your own. Only stand up your
own server, or scaffold a competing entry page, if that target turns out to be
absent.

Tool use loop:
- inspect before acting;
- call one or more tools with valid JSON inputs;
- read tool results, including error results;
- repair malformed or incomplete tool calls instead of repeating them;
- continue until the task is complete, blocked by a concrete reason, or the user
  cancels.

Shell discipline. For a multiline or heavily-quoted command, do not fight inline
quote escaping across the shell-to-interpreter boundary: write the body to a
script file (`.py`, `.ps1`, or `.sh`) and run that file instead. If a command
fails the same way twice, stop and change approach rather than re-sending it — a
repeated identical error will keep failing. If a needed command-line tool is
missing, say so plainly and surface the gap instead of silently working around
it.

Keep reasoning separate from the final answer. When no more tool calls are
needed, respond with a concise final answer that states what changed and how it
was verified. If stuck, say exactly what blocks progress.",
        tools = names.join(", ")
    )
}

#[cfg(test)]
mod cue_tests {
    use super::*;

    #[test]
    fn the_knowledge_search_cue_appears_only_when_the_tool_is_registered() {
        let with = build_prompt(&["knowledge_search", "read_file"]);
        assert!(
            with.contains("searchable knowledge base"),
            "the cue must be present when knowledge_search is registered"
        );
        assert!(with.contains("knowledge_search"));

        let without = build_prompt(&["read_file", "write_file"]);
        assert!(
            !without.contains("searchable knowledge base"),
            "the cue must be absent when knowledge_search is not registered"
        );
    }

    #[test]
    fn the_remember_cue_appears_only_when_the_tool_is_registered() {
        let with = build_prompt(&["remember", "read_file"]);
        assert!(
            with.contains("call `remember` to propose it"),
            "the cue must be present when remember is registered"
        );
        let without = build_prompt(&["read_file", "write_file"]);
        assert!(
            !without.contains("call `remember`"),
            "the cue must be absent when remember is not registered"
        );
    }

    #[test]
    fn the_skill_drafts_cue_appears_only_when_the_tool_is_registered() {
        let with = build_prompt(&["skill_drafts", "read_file"]);
        assert!(
            with.contains("call `skill_drafts`"),
            "the cue must be present when skill_drafts is registered"
        );
        assert!(
            with.contains("enabling a skill stays a human step"),
            "the cue must keep activation a human step"
        );
        let without = build_prompt(&["read_file", "write_file"]);
        assert!(
            !without.contains("skill drafts"),
            "the cue must be absent when skill_drafts is not registered"
        );
    }

    #[test]
    fn the_skill_search_cue_appears_only_when_the_tool_is_registered() {
        let with = build_prompt(&["skill_search", "skill_load", "read_file"]);
        assert!(
            with.contains("call `skill_search`"),
            "the cue must be present when skill_search is registered"
        );
        assert!(
            with.contains("goes through the normal permission gate"),
            "the cue must keep actions on the permission gate"
        );
        // Absent by default: autonomous discovery is off, so the tool is not
        // registered and the model is not nudged to reach for skills on its own.
        let without = build_prompt(&["read_file", "write_file"]);
        assert!(
            !without.contains("call `skill_search`"),
            "the cue must be absent when skill_search is not registered"
        );
    }

    #[test]
    fn the_tool_search_cue_appears_only_when_the_tool_is_registered() {
        let with = build_prompt(&["tool_search", "tool_load", "read_file"]);
        assert!(
            with.contains("call `tool_search`"),
            "the cue must be present when tool_search is registered"
        );
        assert!(
            with.contains("working subset"),
            "the cue must say the advertised set is a subset"
        );
        assert!(
            with.contains("goes through the normal permission gate"),
            "the cue must keep actions on the permission gate"
        );
        // Absent by default: the broker is off, so the tool is not registered.
        let without = build_prompt(&["read_file", "write_file"]);
        assert!(
            !without.contains("call `tool_search`"),
            "the cue must be absent when tool_search is not registered"
        );
    }

    #[test]
    fn the_marker_cue_is_gated_on_both_the_flag_and_tool_search() {
        // Enabled + tool_search registered: the marker convention appears.
        let on = build_prompt_with(&["tool_search", "tool_load", "read_file"], true);
        assert!(
            on.contains("NEED:"),
            "the marker cue must be present when enabled"
        );
        // Flag off: no marker convention, even with tool_search present.
        let off = build_prompt_with(&["tool_search", "tool_load"], false);
        assert!(
            !off.contains("NEED:"),
            "the marker cue must be off by default"
        );
        // Flag on but no broker (no tool_search): the marker would be inert, so
        // it is not emitted.
        let inert = build_prompt_with(&["read_file", "write_file"], true);
        assert!(
            !inert.contains("NEED:"),
            "the marker cue needs tool_search to be actionable"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_names_every_builtin_tool() {
        let tools = ToolRegistry::with_builtins();
        let prompt = agent_system_prompt(&tools, false);
        for name in tools.names() {
            assert!(prompt.contains(name), "prompt omitted {name}");
        }
        assert!(!prompt.contains("-Plan.md"));
        assert!(!prompt.contains("tasks/"));
    }

    #[test]
    fn prompt_carries_the_look_before_launch_convention() {
        // Always-on: probing a named target uses core tools (fetch/run_shell), so
        // the convention is not gated on an optional tool.
        let prompt = build_prompt(&["read_file", "run_shell"]);
        assert!(
            prompt.contains("Look before you launch"),
            "missing the look-before-launch convention"
        );
        assert!(
            prompt.contains("probe it first"),
            "the convention must steer the model to probe first"
        );
    }

    #[test]
    fn prompt_carries_shell_and_missing_tool_discipline() {
        let prompt = build_prompt(&["read_file"]);
        // Steer multiline/quoted shell to a script file rather than fighting
        // inline escaping.
        assert!(
            prompt.contains("script file"),
            "missing script-file guidance"
        );
        assert!(
            prompt.contains(".ps1"),
            "missing concrete script extensions"
        );
        // Stop repeating an identical failing command.
        assert!(
            prompt.contains("same way twice"),
            "missing repeated-error guidance"
        );
        // Surface a missing tool instead of working around it.
        assert!(
            prompt.contains("missing"),
            "missing the absent-tool guidance"
        );
    }
}
