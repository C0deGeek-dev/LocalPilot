//! The tool registry and permission-gated dispatch.

use localpilot_config::redact::redact;
use localpilot_core::{ToolCall, ToolOutcome, ToolResult};
use localpilot_sandbox::{Approver, Decision, PermissionEngine, PermissionRequest, Profile};
use serde_json::Value;

use crate::contract::{Confirmation, Reversibility};

use crate::builtins::{
    AppendFile, ApplyPatch, Delegate, EditFile, Fetch, FindFiles, GitAdd, GitCommit, GitDiff,
    GitLog, GitRestore, GitStatus, ListFiles, MultiEdit, ReadFile, ReadToolOutput, ReplaceInFile,
    SearchText, UpdatePlan, WriteFile,
};
use crate::builtins_ask::AskUser;
use crate::builtins_background::RunBackground;
use crate::builtins_shell::RunShell;
use crate::builtins_swarm::Swarm;
use crate::catalog::{Catalog, ToolSource};
use crate::tool::{GateVerdict, Tool, ToolContext, ToolGate};

/// Context-size bound on a tool result. Output beyond this is kept as head +
/// tail in context, with the full text spilled to the retention store under
/// the call id so `read_tool_output` can fetch it.
const CONTEXT_OUTPUT_BYTES: usize = 16 * 1024;
/// How much of the tail survives in context when output is bounded.
const CONTEXT_TAIL_BYTES: usize = 2 * 1024;

/// A set of tools. Dispatch is the single entry point: it authorizes every effect
/// through the permission engine before invoking a tool and redacts every output,
/// so neither the model nor the harness can reach a side effect another way.
pub struct ToolRegistry {
    tools: Vec<std::sync::Arc<dyn Tool>>,
    /// Provenance of each tool, kept in lockstep with `tools`, so the catalog
    /// projection can discriminate a builtin from a specific MCP server's tool.
    sources: Vec<ToolSource>,
}

impl ToolRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tools: Vec::new(),
            sources: Vec::new(),
        }
    }

    /// A registry with all builtin tools.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        registry.register(Box::new(ReadFile));
        registry.register(Box::new(WriteFile));
        registry.register(Box::new(AppendFile));
        registry.register(Box::new(EditFile));
        registry.register(Box::new(MultiEdit));
        registry.register(Box::new(ReplaceInFile));
        registry.register(Box::new(ListFiles));
        registry.register(Box::new(FindFiles));
        registry.register(Box::new(SearchText));
        registry.register(Box::new(ApplyPatch));
        registry.register(Box::new(RunShell));
        registry.register(Box::new(RunBackground));
        registry.register(Box::new(Fetch));
        registry.register(Box::new(ReadToolOutput));
        registry.register(Box::new(GitStatus));
        registry.register(Box::new(GitDiff));
        registry.register(Box::new(GitLog));
        registry.register(Box::new(GitAdd));
        registry.register(Box::new(GitRestore));
        registry.register(Box::new(GitCommit));
        registry.register(Box::new(Delegate));
        registry.register(Box::new(AskUser));
        registry.register(Box::new(Swarm));
        registry.register(Box::new(UpdatePlan));
        registry
    }

    /// Add a builtin tool.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.register_from(tool, ToolSource::Builtin);
    }

    /// Add a tool from a known source (a builtin, or a specific MCP server). The
    /// source feeds the catalog projection and the content fingerprint; it does
    /// not change dispatch or permission behaviour in any way.
    pub fn register_from(&mut self, tool: Box<dyn Tool>, source: ToolSource) {
        self.tools.push(std::sync::Arc::from(tool));
        self.sources.push(source);
    }

    /// Project the current registry into a live, fingerprinted [`Catalog`] — the
    /// searchable surface the pull-discovery broker resolves needs against. The
    /// catalog is derived and disposable; the registry stays the source of truth.
    #[must_use]
    pub fn catalog(&self) -> Catalog {
        Catalog::project(self.tools.iter().zip(&self.sources).map(|(tool, source)| {
            (
                tool.name().to_string(),
                tool.description().to_string(),
                tool.schema(),
                source.clone(),
            )
        }))
    }

    /// Look up a tool by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(AsRef::as_ref)
    }

    /// Whether the named tool is served by an MCP server (vs. a builtin). An MCP
    /// tool has no typed schema to guide a safe repair, so the argument-repair
    /// stage refuses it. An unknown name reports `false`.
    #[must_use]
    pub fn is_mcp(&self, name: &str) -> bool {
        self.tools
            .iter()
            .zip(&self.sources)
            .find(|(tool, _)| tool.name() == name)
            .is_some_and(|(_, source)| matches!(source, ToolSource::Mcp(_)))
    }

    /// A registry holding only the named tools, sharing the same tool instances.
    ///
    /// This is how a child session's tool set is built: by **filtering the
    /// parent's own registry**, never by assembling a new one from the catalog.
    /// The distinction is the whole containment story — filtering can only ever
    /// remove, so a child cannot end up with a tool its parent did not hold, and
    /// no runtime check has to be remembered for that to stay true.
    #[must_use]
    pub fn narrowed(&self, keep: &[String]) -> Self {
        let mut tools = Vec::new();
        let mut sources = Vec::new();
        for (tool, source) in self.tools.iter().zip(&self.sources) {
            if keep.iter().any(|name| name == tool.name()) {
                tools.push(std::sync::Arc::clone(tool));
                sources.push(source.clone());
            }
        }
        Self { tools, sources }
    }

    /// The registered tool names.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.tools.iter().map(|t| t.name()).collect()
    }

    /// The registered tools' name + JSON schema pairs.
    #[must_use]
    pub fn schemas(&self) -> Vec<(&str, Value)> {
        self.tools.iter().map(|t| (t.name(), t.schema())).collect()
    }

    /// The registered tools' name, description, and JSON schema, for building
    /// provider tool specifications.
    #[must_use]
    pub fn specs(&self) -> Vec<(&str, &str, Value)> {
        self.tools
            .iter()
            .map(|t| (t.name(), t.description(), t.schema()))
            .collect()
    }

    /// Dispatch a tool call: authorize every effect, invoke, then redact. A
    /// failure or denial is returned as an error [`ToolResult`], never a panic.
    pub async fn dispatch(
        &self,
        call: &ToolCall,
        ctx: &ToolContext<'_>,
        engine: &PermissionEngine,
        approver: &dyn Approver,
    ) -> ToolResult {
        self.dispatch_gated(call, ctx, engine, approver, &[]).await
    }

    /// [`ToolRegistry::dispatch`] with additional tighten-only gates consulted
    /// *after* the permission engine. The engine is the always-on first link:
    /// gates run only for calls it (and the user) already authorized, and can
    /// only block, never grant.
    pub async fn dispatch_gated(
        &self,
        call: &ToolCall,
        ctx: &ToolContext<'_>,
        engine: &PermissionEngine,
        approver: &dyn Approver,
        gates: &[&dyn ToolGate],
    ) -> ToolResult {
        let Some(tool) = self.get(&call.name) else {
            return unusable_result(
                &call.name,
                &call.id,
                &format!("unknown tool: {}", call.name),
                ctx,
            );
        };

        let effects = match tool.effects(&call.input, ctx) {
            Ok(effects) => effects,
            Err(err) => return unusable_result(tool.name(), &call.id, &err.to_string(), ctx),
        };

        // Reversibility-aware confirmation: an irreversible tool (or one that
        // asks to always confirm) raises an `Allow` to `Ask`, so even the
        // relaxed profile pauses for a destructive, un-undoable action. This is
        // tighten-only — it never turns a `Deny` into anything weaker — and it
        // does not touch `bypass` or `unrestricted`, whose whole point is no
        // prompts.
        let contract = tool.contract();
        let force_confirm = !matches!(engine.profile(), Profile::Bypass | Profile::Unrestricted)
            && (matches!(contract.reversibility, Reversibility::Irreversible)
                || matches!(contract.confirmation, Confirmation::Always));

        // The tool supplies its own approval detail — it knows its schema; the
        // registry does not guess at input keys. Display-only, never decisive.
        let detail = tool.approval_detail(&call.input);
        for effect in &effects {
            let request = PermissionRequest {
                tool: tool.name().to_string(),
                effect: *effect,
                interactivity: ctx.interactivity,
                trusted: ctx.trusted,
                detail: detail.clone(),
            };
            let allowed = match engine.decide(&request) {
                Decision::Allow if force_confirm => approver.approve(&request).await,
                Decision::Allow => true,
                Decision::Ask => approver.approve(&request).await,
                Decision::Deny => false,
            };
            if !allowed {
                return unusable_result(
                    tool.name(),
                    &call.id,
                    &denial_message(tool.name(), &request),
                    ctx,
                );
            }
        }

        for gate in gates {
            if let GateVerdict::Block { reason } = gate.check(call, &effects) {
                return unusable_result(
                    tool.name(),
                    &call.id,
                    &format!("blocked by {}: {reason}", gate.name()),
                    ctx,
                );
            }
        }

        match tool.invoke(call.input.clone(), ctx).await {
            // Redaction happens here, for every profile including bypass.
            Ok(output) => {
                let redacted = redact(&output.text);
                let bounded = bound_output(tool.name(), &call.id, &redacted, ctx);
                ToolResult {
                    id: call.id.clone(),
                    output: format_tool_output(tool.name(), &bounded, output.outcome),
                    outcome: output.outcome,
                }
            }
            Err(err) => unusable_result(tool.name(), &call.id, &err.to_string(), ctx),
        }
    }
}

/// The single exit for every result the model sees without the tool having
/// produced a normal output: tool errors and the registry's own synthesized
/// refusals (unknown tool, effects error, denial, gate block). An error is
/// model-visible data like any other result, so it takes the same redaction
/// and the same context bound as the success arm — the safety invariant in
/// `docs/05-tool-system.md` holds per result, not per happy path.
fn unusable_result(
    tool_name: &str,
    call_id: &localpilot_core::ToolUseId,
    text: &str,
    ctx: &ToolContext<'_>,
) -> ToolResult {
    let redacted = redact(text);
    let bounded = bound_output(tool_name, call_id, &redacted, ctx);
    ToolResult::error(
        call_id.clone(),
        format_tool_output(tool_name, &bounded, ToolOutcome::Unusable),
    )
}

/// The model-visible text for a denied tool call. An out-of-workspace path
/// denial names the target and every way the user can grant the access, so it
/// is an actionable answer instead of a dead end.
fn denial_message(tool: &str, request: &PermissionRequest) -> String {
    let mut message = format!("permission denied for {tool}");
    if request.effect.is_outside_workspace() {
        if !request.detail.is_empty() {
            message.push_str(&format!(" ({})", request.detail));
        }
        message.push_str(
            ": the path is outside the workspace. The user can approve the prompt in an \
             interactive session, grant standing read access by listing the directory in \
             `extra_read_roots` under `[permissions]` in .localpilot.toml, or relaunch with \
             `--permission unrestricted`.",
        );
    }
    message
}

/// Bound an output to the context budget: keep the head and tail, spill the
/// full (already redacted) text to the retention store under the call id, and
/// say so explicitly — truncation is never silent.
fn bound_output(
    tool: &str,
    id: &localpilot_core::ToolUseId,
    text: &str,
    ctx: &ToolContext<'_>,
) -> String {
    if text.len() <= CONTEXT_OUTPUT_BYTES || tool == "read_tool_output" {
        return text.to_string();
    }
    let retention_note = match ctx.retention {
        Some(retention) => {
            let key = retention_key(id.as_str());
            match retention.retain(&key, text) {
                Ok(()) => {
                    format!("full output retained under id {key}; use read_tool_output to fetch it")
                }
                Err(reason) => format!("full output could not be retained: {reason}"),
            }
        }
        None => "full output was not retained in this session".to_string(),
    };
    let head_end = floor_char_boundary(text, CONTEXT_OUTPUT_BYTES - CONTEXT_TAIL_BYTES);
    let tail_start = floor_char_boundary(text, text.len() - CONTEXT_TAIL_BYTES);
    format!(
        "{}\n... [output truncated: {} of {} bytes shown; {}] ...\n{}",
        &text[..head_end],
        CONTEXT_OUTPUT_BYTES,
        text.len(),
        retention_note,
        &text[tail_start..]
    )
}

/// A retention key derived from the provider-assigned call id, restricted to
/// storage-safe characters.
fn retention_key(call_id: &str) -> String {
    let cleaned: String = call_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .collect();
    if cleaned.is_empty() {
        "tool-output".to_string()
    } else {
        cleaned
    }
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn format_tool_output(tool: &str, output: &str, outcome: ToolOutcome) -> String {
    format!(
        "tool: {tool}\nstatus: {}\noutput:\n{output}",
        outcome.status_label()
    )
}

#[cfg(test)]
mod retention_tests {
    use super::*;
    use crate::tool::OutputRetention;
    use async_trait::async_trait;
    use localpilot_core::{ToolCall, ToolUseId};
    use localpilot_sandbox::{
        Effect, Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace,
    };
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// An in-memory retention store standing in for the host's disk-backed one.
    #[derive(Default)]
    struct MemoryRetention(Mutex<HashMap<String, String>>);

    impl crate::tool::OutputRetention for MemoryRetention {
        fn retain(&self, id: &str, output: &str) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .insert(id.to_string(), output.to_string());
            Ok(())
        }
        fn fetch(&self, id: &str) -> Result<Option<String>, String> {
            Ok(self.0.lock().unwrap().get(id).cloned())
        }
    }

    /// A tool that emits a large output through `cap`, exactly as a real builtin
    /// (`search_text`, `run_shell`, …) does — the seam the fix touches.
    struct BigTool(String);

    #[async_trait]
    impl Tool for BigTool {
        fn name(&self) -> &str {
            "big"
        }
        fn description(&self) -> &str {
            "emits a large output"
        }
        fn schema(&self) -> Value {
            json!({ "type": "object" })
        }
        fn effects(
            &self,
            _input: &Value,
            _ctx: &ToolContext<'_>,
        ) -> Result<Vec<Effect>, crate::error::ToolError> {
            Ok(Vec::new())
        }
        async fn invoke(
            &self,
            _input: Value,
            _ctx: &ToolContext<'_>,
        ) -> Result<crate::ToolOutput, crate::error::ToolError> {
            Ok(crate::builtins::cap(self.0.clone()))
        }
    }

    #[tokio::test]
    async fn output_over_the_per_tool_cap_is_retained_in_full_and_readable() {
        // A ~128 KiB output — well past the old 64 KiB per-tool cap — with a unique
        // marker on its own final line. Before the fix the per-tool cap dropped
        // everything past 64 KiB *before* retention, so the tail marker never
        // reached the store and `read_tool_output` could not recover it.
        let tail = "END-OF-BIG-OUTPUT-MARKER";
        let body = format!("{}\n", "a".repeat(63)).repeat(2000);
        let tail_line = 2001; // the 2000 body lines, then the marker on line 2001
        let big = format!("{body}{tail}");
        assert!(big.len() > 64 * 1024);

        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let retention = MemoryRetention::default();
        let ctx = ToolContext {
            workspace: &ws,
            interactivity: Interactivity::NonInteractive,
            trusted: true,
            retention: Some(&retention),
            processes: None,
            agents: None,
            prompter: None,
            peers: None,
        };

        let mut registry = ToolRegistry::new();
        registry.register(Box::new(BigTool(big.clone())));
        registry.register(Box::new(crate::builtins::ReadToolOutput));
        let engine = PermissionEngine::new(Profile::Bypass, Vec::new());
        let approver = ScriptedApprover::always();

        // An alphanumeric call id so the retention key equals it (retention_key
        // only strips storage-unsafe characters).
        let call = ToolCall::new(ToolUseId::new("bigcall1"), "big", json!({}));
        let result = registry.dispatch(&call, &ctx, &engine, &approver).await;
        assert!(!result.is_error());
        assert!(
            result.output.contains("retained under id bigcall1"),
            "the bounded result must point at the retained output: {}",
            result.output
        );

        // The store holds the FULL output, tail included — the whole point.
        let stored = retention.fetch("bigcall1").unwrap().unwrap();
        assert_eq!(stored.len(), big.len(), "the full output must be retained");
        assert!(
            stored.ends_with(tail),
            "content past the old 64 KiB cap must be retained"
        );

        // And `read_tool_output` can page to the tail via the line range (the
        // marker is on the last line).
        let read = ToolCall::new(
            ToolUseId::new("readback1"),
            "read_tool_output",
            json!({ "id": "bigcall1", "start_line": tail_line, "end_line": tail_line }),
        );
        let readback = registry.dispatch(&read, &ctx, &engine, &approver).await;
        assert!(!readback.is_error());
        assert!(
            readback.output.contains(tail),
            "read_tool_output must recover the tail: {}",
            readback.output
        );
    }
}

#[cfg(test)]
mod catalog_tests {
    use super::*;
    use async_trait::async_trait;
    use localpilot_sandbox::Effect;
    use serde_json::json;

    /// A minimal tool used to drive catalog projection without a live workspace.
    struct FakeTool {
        name: &'static str,
        description: &'static str,
    }

    #[async_trait]
    impl Tool for FakeTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            self.description
        }
        fn schema(&self) -> Value {
            json!({ "type": "object" })
        }
        fn effects(
            &self,
            _input: &Value,
            _ctx: &ToolContext<'_>,
        ) -> Result<Vec<Effect>, crate::error::ToolError> {
            Ok(Vec::new())
        }
        async fn invoke(
            &self,
            _input: Value,
            _ctx: &ToolContext<'_>,
        ) -> Result<crate::ToolOutput, crate::error::ToolError> {
            Ok(crate::ToolOutput::ok(""))
        }
    }

    #[test]
    fn catalog_projects_one_entry_per_tool_and_tags_its_source() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(FakeTool {
            name: "alpha",
            description: "first",
        }));
        registry.register_from(
            Box::new(FakeTool {
                name: "beta",
                description: "second",
            }),
            ToolSource::Mcp("files".to_string()),
        );

        let catalog = registry.catalog();
        assert_eq!(catalog.len(), registry.names().len());
        assert_eq!(
            catalog.get("alpha").map(|e| &e.source),
            Some(&ToolSource::Builtin)
        );
        assert_eq!(
            catalog.get("beta").map(|e| &e.source),
            Some(&ToolSource::Mcp("files".to_string()))
        );
    }

    #[test]
    fn is_mcp_distinguishes_an_mcp_tool_from_a_builtin() {
        // The repair stage refuses MCP tools (no typed schema); the registry is how
        // it tells an MCP tool from a builtin.
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(FakeTool {
            name: "builtin_tool",
            description: "b",
        }));
        registry.register_from(
            Box::new(FakeTool {
                name: "mcp_tool",
                description: "m",
            }),
            ToolSource::Mcp("server".to_string()),
        );
        assert!(!registry.is_mcp("builtin_tool"));
        assert!(registry.is_mcp("mcp_tool"));
        assert!(!registry.is_mcp("unknown"), "an unknown tool is not MCP");
    }

    #[test]
    fn rebuilding_without_a_tool_drops_it_from_the_catalog() {
        // A registry rebuild that no longer registers a tool (e.g. an MCP server
        // that stopped advertising it) drops it; the catalog delta says `removed`.
        let mut before = ToolRegistry::new();
        before.register(Box::new(FakeTool {
            name: "keep",
            description: "stays",
        }));
        before.register(Box::new(FakeTool {
            name: "gone",
            description: "leaves",
        }));

        let mut after = ToolRegistry::new();
        after.register(Box::new(FakeTool {
            name: "keep",
            description: "stays",
        }));

        let delta = before.catalog().delta(&after.catalog());
        assert_eq!(delta.removed, vec!["gone".to_string()]);
        assert!(delta.added.is_empty());
        assert!(after.catalog().get("gone").is_none());
    }
}

#[cfg(test)]
mod narrowing_tests {
    use super::*;

    #[test]
    fn narrowing_can_only_remove() {
        let full = ToolRegistry::with_builtins();
        let all: Vec<String> = full.names().iter().map(|n| (*n).to_string()).collect();

        // A name the registry does not hold cannot be conjured into existence.
        let mut asked = all.clone();
        asked.push("teleport".to_string());
        let narrowed = full.narrowed(&asked);
        assert_eq!(
            narrowed.names().len(),
            all.len(),
            "asking for an unheld tool must not add one"
        );
        assert!(!narrowed.names().contains(&"teleport"));

        // Every narrowing is a subset of the source registry.
        let subset = full.narrowed(&["read_file".to_string(), "search_text".to_string()]);
        assert_eq!(subset.names().len(), 2);
        for name in subset.names() {
            assert!(full.names().contains(&name), "{name} was not in the parent");
        }
    }

    #[test]
    fn narrowing_to_nothing_yields_an_empty_registry() {
        let full = ToolRegistry::with_builtins();
        assert!(full.narrowed(&[]).names().is_empty());
    }

    #[test]
    fn a_narrowed_registry_keeps_each_tools_provenance() {
        let full = ToolRegistry::with_builtins();
        let narrowed = full.narrowed(&["read_file".to_string()]);
        assert_eq!(narrowed.specs().len(), 1, "the spec projection still works");
    }
}
