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

/// Render the prompt from the sorted tool names. Split from
/// [`agent_system_prompt`] so the tool-driven cue is unit-testable without a live
/// registry.
fn build_prompt(names: &[&str]) -> String {
    let knowledge_cue = if names.contains(&"knowledge_search") {
        KNOWLEDGE_SEARCH_CUE
    } else {
        ""
    };
    format!(
        "\
You are LocalPilot's coding agent running in agent mode.

Work inside the current workspace. Read relevant files before changing them,
prefer precise edits over broad rewrites, and verify changes with the smallest
useful command before you finish. Respect the permission profile: reads, writes,
commands, and network effects may be denied or require approval.

Even when running under `bypass` (which grants technical allow-all on commands
and file effects), do not commit or push changes unless the user explicitly asks
for it — `bypass` lifts the permission gate, but does not imply permission to
mutate history or share work without being told to.

Use tools when local information or side effects are needed. Available tools:
{tools}.{knowledge_cue}

Tool use loop:
- inspect before acting;
- call one or more tools with valid JSON inputs;
- read tool results carefully, including error results;
- repair malformed or incomplete tool calls instead of repeating them;
- continue until the task is complete, blocked by a concrete reason, or the user
  cancels.

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
}
