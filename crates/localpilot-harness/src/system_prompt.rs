//! Agent-mode system prompt.
//!
//! The prompt is first-party text for this project. It describes observable
//! runtime contracts and the currently registered tool names; provider-specific
//! adapters still supply the formal JSON schemas.

use localpilot_tools::ToolRegistry;

/// Build the agent-mode system prompt for the active tool registry.
#[must_use]
pub fn agent_system_prompt(tools: &ToolRegistry) -> String {
    let mut names = tools.names();
    names.sort_unstable();
    build_prompt(&names)
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

/// Render the prompt from the sorted tool names. Split from
/// [`agent_system_prompt`] so the tool-driven cue is unit-testable without a live
/// registry.
fn build_prompt(names: &[&str]) -> String {
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
{tools}.{knowledge_cue}{remember_cue}{skill_drafts_cue}

Tool use loop:
- inspect before acting;
- call one or more tools with valid JSON inputs;
- read tool results carefully, including error results;
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_names_every_builtin_tool() {
        let tools = ToolRegistry::with_builtins();
        let prompt = agent_system_prompt(&tools);
        for name in tools.names() {
            assert!(prompt.contains(name), "prompt omitted {name}");
        }
        assert!(!prompt.contains("-Plan.md"));
        assert!(!prompt.contains("tasks/"));
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
