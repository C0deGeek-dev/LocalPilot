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
    format!(
        "\
You are LocalPilot's coding agent running in agent mode.

Work inside the current workspace. Read relevant files before changing them,
prefer precise edits over broad rewrites, and verify changes with the smallest
useful command before you finish. Respect the permission profile: reads, writes,
commands, and network effects may be denied or require approval.

Use tools when local information or side effects are needed. Available tools:
{tools}.

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
