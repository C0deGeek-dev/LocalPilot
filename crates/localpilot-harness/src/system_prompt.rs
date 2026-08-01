//! Agent-mode system prompt.
//!
//! The prompt is first-party text for this project. It describes observable
//! runtime contracts and the currently registered tool names; provider-specific
//! adapters still supply the formal JSON schemas.

use localpilot_agents::PromptParts;
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
    compose_with(
        &names,
        marker_enabled,
        PromptParts::all(),
        has_doc_tool(tools),
    )
}

/// Whether the registry advertises a tool that serves documentation. Judged by
/// the shared, vendor-neutral capability vocabulary, over each tool's own name
/// and description — no server or library is named anywhere in this decision.
fn has_doc_tool(tools: &ToolRegistry) -> bool {
    tools
        .specs()
        .iter()
        .any(|(name, description, _)| localpilot_tools::describes_documentation(name, description))
}

/// Build a prompt from a *subset* of the sections the main session uses.
///
/// A subagent runs a narrow task with a narrow tool set, and guidance it cannot
/// act on is context it pays for and nothing else: an agent with two read-only
/// tools has no use for editing guidance or shell discipline. Selecting
/// [`PromptParts::all`] reproduces [`agent_system_prompt`] exactly, which is
/// what keeps the two paths from drifting.
#[must_use]
pub fn composed_system_prompt(
    tools: &ToolRegistry,
    marker_enabled: bool,
    parts: PromptParts,
) -> String {
    let mut names = tools.names();
    names.sort_unstable();
    compose_with(&names, marker_enabled, parts, has_doc_tool(tools))
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
/// skill discovery is enabled), that tells the model skills — from the user's
/// global directory or this project — are reachable on demand by search rather
/// than carried in context.
const SKILL_SEARCH_CUE: &str = concat!(
    "\n\n",
    "Your user-global directory or this project may define skills — advisory prompt modules for ",
    "recurring tasks. They are not loaded into context; when a task looks like one, call ",
    "`skill_search` to find relevant skills (you get back names and one-line summaries), then ",
    "`skill_load` to read one and apply its guidance yourself. Loading a skill runs nothing; any ",
    "action it suggests still goes through the normal permission gate.",
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

/// The cue, appended only when `ask_user` is registered, that tells the model it
/// can put a decision to the user — and, just as importantly, when not to. The
/// threshold is part of the text: without it a model starts asking permission
/// for everything, which is worse than the silent guess this replaces.
const ASK_USER_CUE: &str = concat!(
    "

",
    "You can ask the user a question with `ask_user`. Ask when different readings of the request ",
    "would lead to materially different work, or before something hard to undo. Otherwise pick the ",
    "obvious option and state the assumption in your answer — do not ask for permission to do work ",
    "you were already asked to do, and do not ask to report progress. Where no user is reachable ",
    "the tool says so; then choose and say what you assumed.",
);

/// The documentation policy for a session that advertises its full tool set: the
/// suitable tool is already visible, so the guidance is to call it directly and
/// never mentions the broker's discovery surface.
const DOCUMENTATION_CUE_DIRECT: &str = concat!(
    "

",
    "When a task depends on current or version-specific behaviour of an external library, ",
    "framework, SDK, API, CLI, or cloud service, consult current documentation rather than ",
    "relying on what you remember. Upgrade errors, migration failures, deprecated APIs, changed ",
    "configuration shapes, and version mismatches are strong signals that your prior knowledge is ",
    "stale. Inspect the project first to identify the dependency, its installed version, its ",
    "configuration, and the exact error, then call the most suitable documentation tool listed ",
    "above. Stable local implementation questions need no documentation lookup. If no suitable ",
    "tool can answer, continue from local evidence and say that current documentation could not ",
    "be verified.",
);

/// The documentation policy for a brokered session, where a suitable tool may be
/// hidden behind discovery: the same threshold, routed through the reveal flow.
const DOCUMENTATION_CUE_BROKERED: &str = concat!(
    "

",
    "When a task depends on current or version-specific behaviour of an external library, ",
    "framework, SDK, API, CLI, or cloud service, consult current documentation rather than ",
    "relying on what you remember. Upgrade errors, migration failures, deprecated APIs, changed ",
    "configuration shapes, and version mismatches are strong signals that your prior knowledge is ",
    "stale. Inspect the project first to identify the dependency, its installed version, its ",
    "configuration, and the exact error, then call `tool_search` with the capability you need ",
    "(for example `current documentation for a library version`), reveal the best match with ",
    "`tool_load`, and call it normally. Stable local implementation questions need no ",
    "documentation lookup. If no suitable tool can answer, continue from local evidence and say ",
    "that current documentation could not be verified.",
);

/// Render the prompt from the sorted tool names with the marker nudge off. A
/// test-only convenience over [`build_prompt_with`] so the existing cue tests stay
/// terse; production code calls [`agent_system_prompt`].
#[cfg(test)]
fn build_prompt(names: &[&str]) -> String {
    build_prompt_with(names, false)
}

/// Render the prompt, optionally adding the `NEED:` marker convention.
#[cfg(test)]
fn build_prompt_with(names: &[&str], marker_enabled: bool) -> String {
    compose_with(names, marker_enabled, PromptParts::all(), false)
}

/// [`compose_with`] with no documentation tool advertised — the common case in
/// the section tests, which pass bare tool names.
#[cfg(test)]
fn compose(names: &[&str], marker_enabled: bool, parts: PromptParts) -> String {
    compose_with(names, marker_enabled, parts, false)
}

/// The agent-mode opening. Always present when `include_base` is on; it is the
/// only section that establishes what the model is.
const BASE_SECTION: &str = "You are LocalPilot's coding agent running in agent mode.

Work inside the current workspace. Read relevant files before changing them,
prefer precise edits over broad rewrites, and verify changes with the smallest
useful command before you finish.";

/// Which write tool to reach for, and the modular-file preference.
const EDITING_SECTION: &str = "To change an existing file, default to
`replace_in_file` (replace an exact block of old text with new text — it may
span multiple lines); use `apply_patch` for changes across several files or
that create and delete files. Reserve `write_file` for a brand-new file or a
full rewrite of one file — do not use it to make a small edit.

Split a large implementation across several small, focused files rather than
emitting one enormous file — modular files read better and keep each tool call
small enough to send reliably. Treat 'keep it in one file' as a preference, not
a hard rule: split a web app into separate HTML, CSS, and JS files once one file
would grow too large.";

/// Permission-profile framing and commit etiquette. Turning this off is a
/// deliberate choice a definition has to make explicitly.
const SAFETY_SECTION: &str = "Respect the
permission profile: reads, writes, commands, and network effects may be denied
or require approval.

Even when running under `bypass` (which grants technical allow-all on commands
and file effects), do not commit or push changes unless the user explicitly asks
for it — `bypass` lifts the permission gate, but does not imply permission to
mutate history or share work without being told to.";

/// Inspect-before-launch.
const LOOK_BEFORE_LAUNCH_SECTION: &str =
    "Look before you launch. If a task names an existing target you can reach — a URL,
a running service, a `host:port` — inspect or probe it first (for example fetch or
curl it) before assuming you must create or launch your own. Only stand up your
own server, or scaffold a competing entry page, if that target turns out to be
absent.";

/// The tool-use loop and shell discipline.
const TOOL_LOOP_SECTION: &str = "Tool use loop:
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
it.";

/// The closing instruction. Always present: without it a model has no contract
/// for how to end a turn.
const CLOSING_SECTION: &str =
    "Keep reasoning separate from the final answer. When no more tool calls are
needed, respond with a concise final answer that states what changed and how it
was verified. If stuck, say exactly what blocks progress.";

/// Assemble the selected sections plus the tool list and its cues.
fn compose_with(
    names: &[&str],
    marker_enabled: bool,
    parts: PromptParts,
    doc_tool_advertised: bool,
) -> String {
    let ask_user_enabled = parts.include_ask_user;
    let mut sections: Vec<String> = Vec::new();
    if parts.include_base {
        sections.push(BASE_SECTION.to_string());
    }
    if parts.include_editing_guidance {
        sections.push(EDITING_SECTION.to_string());
    }
    if parts.include_safety {
        sections.push(SAFETY_SECTION.to_string());
    }
    sections.push(tools_section(
        names,
        marker_enabled,
        doc_tool_advertised,
        ask_user_enabled,
    ));
    if parts.include_look_before_launch {
        sections.push(LOOK_BEFORE_LAUNCH_SECTION.to_string());
    }
    if parts.include_tool_instructions {
        sections.push(TOOL_LOOP_SECTION.to_string());
    }
    sections.push(CLOSING_SECTION.to_string());
    sections.join(
        "

",
    )
}

/// The available-tools line plus every cue gated on a registered tool name.
fn tools_section(
    names: &[&str],
    marker_enabled: bool,
    doc_tool_advertised: bool,
    ask_user_enabled: bool,
) -> String {
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
    // The documentation policy needs a way to reach a documentation tool: either
    // one is advertised outright, or the broker can reveal one. With neither,
    // the guidance would be an instruction the model cannot follow.
    // Gated on both the definition's flag and the tool being registered: a
    // subagent has no prompter, so telling it to ask would be a dead end.
    let ask_user_cue = if ask_user_enabled && names.contains(&"ask_user") {
        ASK_USER_CUE
    } else {
        ""
    };
    let documentation_cue = if names.contains(&"tool_search") {
        DOCUMENTATION_CUE_BROKERED
    } else if doc_tool_advertised {
        DOCUMENTATION_CUE_DIRECT
    } else {
        ""
    };
    format!(
        "Use tools when local information or side effects are needed. Available tools: {tools}.{knowledge_cue}{remember_cue}{skill_drafts_cue}{skill_search_cue}{tool_search_cue}{tool_marker_cue}{ask_user_cue}{documentation_cue}",
        tools = names.join(", ")
    )
}

#[cfg(test)]
mod cue_tests {
    use super::*;

    #[test]
    fn the_prompt_steers_toward_splitting_large_work_into_modular_files() {
        let prompt = build_prompt(&["write_file", "replace_in_file"]);
        assert!(
            prompt.contains("Split a large implementation"),
            "always-on modular-file guidance must be present"
        );
        assert!(prompt.contains("separate HTML, CSS, and JS files"));
    }

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

#[cfg(test)]
mod composition_tests {
    use super::*;

    /// Every sentence the pre-split prompt opened a paragraph with. The
    /// all-parts composition must still contain all of them: the split changed
    /// where paragraph boundaries fall, never what the model is told.
    const ORIGINAL_OPENERS: &[&str] = &[
        "You are LocalPilot's coding agent running in agent mode.",
        "Work inside the current workspace.",
        "To change an existing file, default to",
        "Respect the\npermission profile:",
        "Split a large implementation",
        "Even when running under `bypass`",
        "Use tools when local information or side effects are needed.",
        "Look before you launch.",
        "Tool use loop:",
        "Shell discipline.",
        "Keep reasoning separate from the final answer.",
    ];

    fn all_parts(names: &[&str]) -> String {
        compose(names, false, PromptParts::all())
    }

    #[test]
    fn selecting_every_part_keeps_all_of_the_original_guidance() {
        let prompt = all_parts(&["read_file", "write_file"]);
        for opener in ORIGINAL_OPENERS {
            assert!(
                prompt.contains(opener),
                "the split dropped guidance that used to be present: {opener:?}"
            );
        }
    }

    #[test]
    fn the_default_path_and_the_all_parts_composition_are_the_same_bytes() {
        let names = ["read_file", "search_text"];
        assert_eq!(
            build_prompt_with(&names, false),
            all_parts(&names),
            "the main session must go through exactly the same composition as a \
             subagent that selects everything, or the two paths will drift"
        );
    }

    #[test]
    fn a_narrow_agent_drops_the_guidance_it_cannot_act_on() {
        let parts = PromptParts {
            include_editing_guidance: false,
            include_tool_instructions: false,
            ..PromptParts::all()
        };
        let prompt = compose(&["read_file"], false, parts);
        assert!(
            !prompt.contains("replace_in_file"),
            "editing guidance dropped"
        );
        assert!(
            !prompt.contains("Shell discipline"),
            "shell discipline dropped"
        );
        assert!(
            prompt.contains("Respect the\npermission profile"),
            "safety is still on: {prompt}"
        );
        assert!(
            prompt.contains("Available tools: read_file"),
            "the tool list is never optional: {prompt}"
        );
    }

    #[test]
    fn the_closing_contract_and_the_tool_list_are_never_droppable() {
        // Every part off that can be off.
        let parts = PromptParts {
            include_base: false,
            include_editing_guidance: false,
            include_safety: false,
            include_tool_instructions: false,
            include_look_before_launch: false,
            include_ask_user: false,
        };
        let prompt = compose(&["read_file"], false, parts);
        assert!(
            prompt.contains("Keep reasoning separate"),
            "a model with no closing contract has no way to end a turn: {prompt}"
        );
        assert!(prompt.contains("Available tools: read_file"), "{prompt}");
        assert!(
            !prompt.contains("agent mode"),
            "base really is off: {prompt}"
        );
    }

    #[test]
    fn turning_safety_off_removes_exactly_the_safety_text() {
        let parts = PromptParts {
            include_safety: false,
            ..PromptParts::all()
        };
        let prompt = compose(&["run_shell"], false, parts);
        assert!(!prompt.contains("permission profile"), "{prompt}");
        assert!(!prompt.contains("bypass"), "{prompt}");
        assert!(
            prompt.contains("Tool use loop"),
            "unrelated parts stay: {prompt}"
        );
    }

    #[test]
    fn the_cues_still_gate_on_registered_tool_names_after_the_split() {
        let with = all_parts(&["knowledge_search"]);
        assert!(with.contains("searchable knowledge base"));
        let without = all_parts(&["read_file"]);
        assert!(!without.contains("searchable knowledge base"));
    }

    #[test]
    fn sections_are_separated_by_exactly_one_blank_line() {
        let prompt = all_parts(&["read_file"]);
        assert!(
            !prompt.contains("\n\n\n"),
            "joining introduced a triple newline: {prompt:?}"
        );
    }

    /// A synthetic registry holding one generically-described documentation
    /// tool. Nothing here names a real MCP server, product, or library.
    fn registry_with_doc_tool() -> ToolRegistry {
        struct DocsTool;

        #[async_trait::async_trait]
        impl localpilot_tools::Tool for DocsTool {
            fn name(&self) -> &str {
                "query"
            }
            fn description(&self) -> &str {
                "Query documentation for a package"
            }
            fn schema(&self) -> serde_json::Value {
                serde_json::json!({ "type": "object" })
            }
            fn effects(
                &self,
                _input: &serde_json::Value,
                _ctx: &localpilot_tools::ToolContext<'_>,
            ) -> Result<Vec<localpilot_sandbox::Effect>, localpilot_tools::ToolError> {
                Ok(Vec::new())
            }
            async fn invoke(
                &self,
                _input: serde_json::Value,
                _ctx: &localpilot_tools::ToolContext<'_>,
            ) -> Result<localpilot_tools::ToolOutput, localpilot_tools::ToolError> {
                Ok(localpilot_tools::ToolOutput::ok("docs"))
            }
        }

        let mut registry = ToolRegistry::new();
        registry.register(Box::new(DocsTool));
        registry
    }

    #[test]
    fn an_advertised_documentation_tool_gets_direct_use_guidance() {
        let prompt = agent_system_prompt(&registry_with_doc_tool(), false);
        assert!(
            prompt.contains("current or version-specific behaviour"),
            "the version-sensitive documentation policy must be present"
        );
        assert!(
            prompt.contains("Upgrade errors, migration failures"),
            "the policy must name the signals that trigger it"
        );
        assert!(
            prompt.contains("call the most suitable documentation tool listed above"),
            "with the full tool set advertised the guidance is direct use"
        );
        assert!(
            !prompt.contains("tool_search"),
            "broker-off guidance must not reference the discovery surface: {prompt}"
        );
    }

    #[test]
    fn a_brokered_session_gets_the_search_then_load_flow() {
        let prompt = build_prompt(&["read_file", "tool_search", "tool_load"]);
        assert!(prompt.contains("current or version-specific behaviour"));
        assert!(
            prompt.contains("call `tool_search`")
                && prompt.contains("`tool_load`")
                && prompt.contains("call it normally"),
            "the brokered policy must describe search → load → call"
        );
    }

    #[test]
    fn the_documentation_policy_is_vendor_neutral() {
        let brokered = build_prompt(&["read_file", "tool_search"]);
        let direct = agent_system_prompt(&registry_with_doc_tool(), false);
        for prompt in [&brokered, &direct] {
            let lower = prompt.to_ascii_lowercase();
            for vendor in ["context7", "prisma", "npm", "pypi", "github"] {
                assert!(
                    !lower.contains(vendor),
                    "the policy must name no vendor, found {vendor}"
                );
            }
        }
    }

    #[test]
    fn no_documentation_policy_without_a_way_to_reach_documentation() {
        // No documentation tool advertised and no broker to reveal one: the
        // guidance would be an instruction the model cannot act on.
        let prompt = build_prompt(&["read_file", "write_file"]);
        assert!(
            !prompt.contains("current or version-specific behaviour"),
            "the policy must be absent when no tool could satisfy it"
        );
    }

    #[test]
    fn the_policy_stays_bounded_to_version_sensitive_work() {
        let prompt = agent_system_prompt(&registry_with_doc_tool(), false);
        assert!(
            prompt.contains("Stable local implementation questions need no documentation lookup"),
            "the policy must state its own threshold"
        );
        assert!(
            prompt.contains("current documentation could not be verified"),
            "the policy must say what to do when no tool can answer"
        );
    }
}
