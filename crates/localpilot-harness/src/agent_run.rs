//! Running a subagent: a bounded child session.
//!
//! A subagent is a **child [`SessionRuntime`]**, not a second loop and not a
//! second process. It is handed:
//!
//! - a tool registry **filtered from the caller's own** — so its tools are a
//!   subset by construction, with no runtime check to forget;
//! - a permission engine built from the caller's **own profile** — so it can
//!   never be evaluated more permissively than its caller;
//! - the caller's `Store` and `Workspace` — so it cannot reach anything the
//!   caller could not;
//! - its own context window and its own prompt.
//!
//! What comes back is a **bounded summary**, not the child's transcript. A
//! subagent that returns everything it read is worse than no subagent: the whole
//! reason to delegate is that the caller's context stays clean.

use std::sync::Arc;

use localpilot_agents::{AgentDefinition, Bindings, Grants};
use localpilot_llm::ModelProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Approver, PermissionEngine, Workspace};
use localpilot_store::Store;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::session::{SessionConfig, SessionRuntime};

/// How deeply subagents may nest. At depth 1 a subagent cannot itself spawn one:
/// recursive delegation is the cheapest way to burn a budget with nothing to
/// show, and raising a ceiling later is easier than recalling a runaway default.
pub const DEFAULT_MAX_DEPTH: u32 = 1;

/// Bound on the summary handed back to the caller, so a verbose child cannot
/// grow the caller's context without limit.
const MAX_SUMMARY_BYTES: usize = 4 * 1024;

/// Why a delegation was refused before it started. Every variant is a normal,
/// reportable outcome — never a panic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpawnRefusal {
    /// The nesting ceiling was reached.
    DepthExceeded { depth: u32, max: u32 },
    /// The caller holds no tool this definition asks for.
    NoTools { agent: String },
}

impl std::fmt::Display for SpawnRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DepthExceeded { depth, max } => write!(
                f,
                "subagents may nest {max} deep; this call is already at depth {depth}. \
                 Do the work directly instead of delegating again."
            ),
            Self::NoTools { agent } => write!(
                f,
                "agent {agent:?} would have no tools in this session — every tool it asks \
                 for is one this session does not hold. Run the work directly, or grant \
                 the session those tools first."
            ),
        }
    }
}

/// What a completed delegation reports back.
#[derive(Clone, Debug)]
pub struct AgentOutcome {
    /// The bounded summary handed to the caller.
    pub summary: String,
    /// Whether the summary was cut to fit [`MAX_SUMMARY_BYTES`].
    pub truncated: bool,
    /// Tools the child was actually given, for the caller's record.
    pub tools: Vec<String>,
    /// Registered tools the definition asked for that the caller did not hold.
    pub narrowed: Vec<String>,
    /// Why the child's turn ended.
    pub stop: String,
}

/// Everything a delegation needs from its caller. Borrowed rather than cloned:
/// a child that had its own `Store` or `Workspace` could reach past its caller.
pub struct AgentContext<'a> {
    pub provider: Arc<dyn ModelProvider>,
    pub parent_tools: &'a localpilot_tools::ToolRegistry,
    pub parent_engine: &'a PermissionEngine,
    pub workspace: &'a Workspace,
    pub store_root: &'a std::path::Path,
    pub config: &'a SessionConfig,
    /// The caller's nesting depth; the child runs at `depth + 1`.
    pub depth: u32,
    pub max_depth: u32,
}

/// Check the ceilings that must hold *before* a child is built.
///
/// # Errors
/// Returns the refusal to report to the caller.
pub fn check_ceilings(
    ctx: &AgentContext<'_>,
    grants: &Grants,
    agent: &str,
) -> Result<(), SpawnRefusal> {
    if ctx.depth >= ctx.max_depth {
        return Err(SpawnRefusal::DepthExceeded {
            depth: ctx.depth,
            max: ctx.max_depth,
        });
    }
    if grants.tools.is_empty() {
        return Err(SpawnRefusal::NoTools {
            agent: agent.to_string(),
        });
    }
    Ok(())
}

/// Build the child's system prompt: the selected host sections, then the
/// definition's own instructions with its placeholders resolved.
#[must_use]
pub fn child_prompt(
    definition: &AgentDefinition,
    child_tools: &localpilot_tools::ToolRegistry,
    marker_enabled: bool,
    workspace: &Workspace,
) -> String {
    let host = crate::system_prompt::composed_system_prompt(
        child_tools,
        marker_enabled,
        definition.prompt_parts,
    );
    let mut names = child_tools.names();
    names.sort_unstable();
    let bindings = Bindings::default()
        .with("agent_name", definition.name.clone())
        .with("workspace", workspace.root().display().to_string())
        .with("tools", names.join(", "));
    let own = localpilot_agents::render_prompt(&definition.prompt, &bindings);
    format!("{host}\n\n{own}")
}

/// Run one delegation to completion and return its bounded summary.
///
/// # Errors
/// Returns a [`SpawnRefusal`] when a ceiling refuses the spawn. A child that
/// errors mid-turn is **not** an error here: it returns an [`AgentOutcome`]
/// whose `stop` says what happened, because "the agent failed" is information
/// the caller should reason about rather than a failed tool call.
pub async fn run_agent(
    definition: &AgentDefinition,
    task: &str,
    grants: &Grants,
    ctx: AgentContext<'_>,
    cancel: &CancellationToken,
) -> Result<AgentOutcome, SpawnRefusal> {
    check_ceilings(&ctx, grants, &definition.name)?;

    // Containment by construction: the child's registry is *filtered from the
    // caller's*, and its engine carries the caller's own profile.
    let child_tools = ctx.parent_tools.narrowed(&grants.tools);
    let engine = PermissionEngine::new(ctx.parent_engine.profile(), Vec::new());
    let prompt = child_prompt(
        definition,
        &child_tools,
        ctx.config.tool_marker_enabled,
        ctx.workspace,
    );

    let config = SessionConfig {
        model: definition
            .model
            .clone()
            .unwrap_or_else(|| ctx.config.model.clone()),
        interactivity: ctx.config.interactivity,
        trusted: ctx.config.trusted,
        context_token_limit: ctx.config.context_token_limit,
        tool_call_budget: ctx.config.tool_call_budget,
        tool_call_budget_max: ctx.config.tool_call_budget_max,
        tool_marker_enabled: ctx.config.tool_marker_enabled,
        enforce_readable_errors: ctx.config.enforce_readable_errors,
        repair_mode: ctx.config.repair_mode,
        turn_timeout: ctx.config.turn_timeout,
        ..SessionConfig::default()
    };

    let mut child = SessionRuntime::new(
        Arc::clone(&ctx.provider),
        child_tools,
        engine,
        Box::new(DenyingApprover) as Box<dyn Approver>,
        Store::open(ctx.store_root),
        ctx.workspace.clone(),
        RecoveryEngine::new(RecoveryBudget::default()),
        config,
        Vec::new(),
    );
    child.replace_system_prompt(prompt);

    let (events, _rx) = broadcast::channel(256);
    let stop = child.run_turn(task, &events, cancel).await;
    let raw = child.last_assistant_text().unwrap_or_default();
    let (summary, truncated) = bound(&raw);

    Ok(AgentOutcome {
        summary,
        truncated,
        tools: grants.tools.clone(),
        narrowed: grants.narrowed.clone(),
        stop: format!("{stop:?}"),
    })
}

/// Cut a child's answer to the summary bound, on a line boundary where possible.
fn bound(text: &str) -> (String, bool) {
    if text.len() <= MAX_SUMMARY_BYTES {
        return (text.to_string(), false);
    }
    let mut end = MAX_SUMMARY_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let cut = text[..end]
        .rfind('\n')
        .filter(|at| *at > MAX_SUMMARY_BYTES / 2)
        .unwrap_or(end);
    (format!("{}\n… [summary truncated]", &text[..cut]), true)
}

/// A child's approver. A subagent never answers its own permission asks: an
/// unanswered ask denies, fail-closed, exactly as a headless session does.
/// Routing asks to the caller's UI attributed to the child is the next step and
/// is deliberately not faked here — silently auto-approving would be the one
/// behaviour that turns delegation into an escalation.
struct DenyingApprover;

impl Approver for DenyingApprover {
    fn approve<'a>(
        &'a self,
        _request: &'a localpilot_sandbox::PermissionRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(async { false })
    }
}

/// The session's implementation of the delegation host.
///
/// Holds borrows of the caller's own collaborators rather than copies: the child
/// is built *from* them, which is what makes its tool set a subset and its
/// permission profile identical rather than merely similar.
pub struct SessionAgentHost<'a> {
    pub agents: &'a localpilot_agents::AgentSet,
    pub provider: Arc<dyn ModelProvider>,
    pub parent_tools: &'a localpilot_tools::ToolRegistry,
    pub parent_engine: &'a PermissionEngine,
    pub workspace: &'a Workspace,
    pub store_root: &'a std::path::Path,
    pub config: &'a SessionConfig,
    pub depth: u32,
    pub cancel: &'a CancellationToken,
}

impl localpilot_tools::AgentHost for SessionAgentHost<'_> {
    fn available(&self) -> Vec<(String, String)> {
        self.agents
            .agents()
            .iter()
            .map(|a| {
                (
                    a.definition.name.clone(),
                    a.definition.description.trim().to_string(),
                )
            })
            .collect()
    }

    fn run<'a>(
        &'a self,
        agent: &'a str,
        task: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>
    {
        Box::pin(async move {
            let Some(found) = self.agents.get(agent) else {
                return Err(format!("no agent named {agent:?}"));
            };
            let definition = &found.definition;

            let registered: Vec<&str> = self.parent_tools.names();
            // The parent's own set is the registry it is running with: a child
            // can never be granted from anywhere else.
            let grants = localpilot_agents::resolve_grants(definition, &registered, &registered)
                .map_err(|e| e.to_string())?;

            let ctx = AgentContext {
                provider: Arc::clone(&self.provider),
                parent_tools: self.parent_tools,
                parent_engine: self.parent_engine,
                workspace: self.workspace,
                store_root: self.store_root,
                config: self.config,
                depth: self.depth,
                max_depth: DEFAULT_MAX_DEPTH,
            };
            match run_agent(definition, task, &grants, ctx, self.cancel).await {
                Ok(outcome) => {
                    let mut text = outcome.summary;
                    if !outcome.narrowed.is_empty() {
                        text.push_str(&format!(
                            "\n\n(note: {} asked for {} this session does not hold, so it ran \
                             without them)",
                            definition.name,
                            outcome.narrowed.join(", ")
                        ));
                    }
                    Ok(text)
                }
                Err(refusal) => Err(refusal.to_string()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_agents::AgentDefinition;

    fn definition(tools: &[&str]) -> AgentDefinition {
        let mut list = String::new();
        for tool in tools {
            list.push_str("  - '");
            list.push_str(tool);
            list.push_str("'\n");
        }
        AgentDefinition::from_yaml(&format!(
            "format_version: 1\nname: probe\ndescription: d\nprompt: Do {{{{agent_name}}}} work.\ntools:\n{}",
            if list.is_empty() { "  []\n".to_string() } else { list }
        ))
        .expect("fixture parses")
    }

    fn grants(tools: &[&str]) -> Grants {
        Grants {
            tools: tools.iter().map(|t| (*t).to_string()).collect(),
            narrowed: Vec::new(),
        }
    }

    fn context<'a>(
        registry: &'a localpilot_tools::ToolRegistry,
        engine: &'a PermissionEngine,
        workspace: &'a Workspace,
        root: &'a std::path::Path,
        config: &'a SessionConfig,
        depth: u32,
    ) -> AgentContext<'a> {
        AgentContext {
            provider: Arc::new(localpilot_llm::FakeProvider::new()),
            parent_tools: registry,
            parent_engine: engine,
            workspace,
            store_root: root,
            config,
            depth,
            max_depth: DEFAULT_MAX_DEPTH,
        }
    }

    #[test]
    fn the_depth_ceiling_refuses_rather_than_panics() {
        let dir = tempfile::tempdir().unwrap();
        let registry = localpilot_tools::ToolRegistry::with_builtins();
        let engine = PermissionEngine::new(localpilot_sandbox::Profile::Default, Vec::new());
        let workspace = Workspace::new(dir.path()).unwrap();
        let config = SessionConfig::default();
        let ctx = context(&registry, &engine, &workspace, dir.path(), &config, 1);
        let refusal = check_ceilings(&ctx, &grants(&["read_file"]), "probe")
            .expect_err("depth 1 is already at the ceiling");
        assert!(matches!(refusal, SpawnRefusal::DepthExceeded { .. }));
        assert!(
            refusal.to_string().contains("directly"),
            "the refusal must tell the caller what to do instead: {refusal}"
        );
    }

    #[test]
    fn an_agent_with_no_usable_tools_is_refused_with_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        let registry = localpilot_tools::ToolRegistry::with_builtins();
        let engine = PermissionEngine::new(localpilot_sandbox::Profile::Default, Vec::new());
        let workspace = Workspace::new(dir.path()).unwrap();
        let config = SessionConfig::default();
        let ctx = context(&registry, &engine, &workspace, dir.path(), &config, 0);
        let refusal =
            check_ceilings(&ctx, &grants(&[]), "probe").expect_err("no tools is a refusal");
        assert!(matches!(refusal, SpawnRefusal::NoTools { .. }));
    }

    #[test]
    fn the_child_prompt_carries_only_the_childs_tools() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        let full = localpilot_tools::ToolRegistry::with_builtins();
        let child = full.narrowed(&["read_file".to_string()]);
        let prompt = child_prompt(&definition(&["read_file"]), &child, false, &workspace);
        assert!(
            prompt.contains("Available tools: read_file"),
            "the child is told about its own tools: {prompt}"
        );
        assert!(
            !prompt.contains("run_shell"),
            "and never about tools it does not have: {prompt}"
        );
        assert!(
            prompt.contains("Do probe work."),
            "the definition's own prompt is appended with placeholders resolved: {prompt}"
        );
    }

    #[test]
    fn a_long_child_answer_is_bounded_and_says_so() {
        let long = "line\n".repeat(MAX_SUMMARY_BYTES);
        let (summary, truncated) = bound(&long);
        assert!(truncated);
        assert!(summary.len() <= MAX_SUMMARY_BYTES + 32);
        assert!(summary.ends_with("[summary truncated]"));
    }

    #[test]
    fn a_short_answer_is_returned_whole() {
        let (summary, truncated) = bound("done");
        assert!(!truncated);
        assert_eq!(summary, "done");
    }
}
