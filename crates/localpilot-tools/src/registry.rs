//! The tool registry and permission-gated dispatch.

use localpilot_config::redact::redact;
use localpilot_core::{ToolCall, ToolResult};
use localpilot_sandbox::{Approver, Decision, PermissionEngine, PermissionRequest};
use serde_json::Value;

use crate::builtins::{
    ApplyPatch, EditFile, Fetch, FindFiles, GitAdd, GitCommit, GitDiff, GitLog, GitRestore,
    GitStatus, ListFiles, MultiEdit, ReadFile, ReadToolOutput, ReplaceInFile, SearchText,
    UpdatePlan, WriteFile,
};
use crate::builtins_shell::RunShell;
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
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    /// A registry with all builtin tools.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        registry.register(Box::new(ReadFile));
        registry.register(Box::new(WriteFile));
        registry.register(Box::new(EditFile));
        registry.register(Box::new(MultiEdit));
        registry.register(Box::new(ReplaceInFile));
        registry.register(Box::new(ListFiles));
        registry.register(Box::new(FindFiles));
        registry.register(Box::new(SearchText));
        registry.register(Box::new(ApplyPatch));
        registry.register(Box::new(RunShell));
        registry.register(Box::new(Fetch));
        registry.register(Box::new(ReadToolOutput));
        registry.register(Box::new(GitStatus));
        registry.register(Box::new(GitDiff));
        registry.register(Box::new(GitLog));
        registry.register(Box::new(GitAdd));
        registry.register(Box::new(GitRestore));
        registry.register(Box::new(GitCommit));
        registry.register(Box::new(UpdatePlan));
        registry
    }

    /// Add a tool.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    /// Look up a tool by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(AsRef::as_ref)
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
            return ToolResult::error(
                call.id.clone(),
                format_tool_output(&call.name, &format!("unknown tool: {}", call.name), true),
            );
        };

        let effects = match tool.effects(&call.input, ctx) {
            Ok(effects) => effects,
            Err(err) => {
                return ToolResult::error(
                    call.id.clone(),
                    format_tool_output(tool.name(), &err.to_string(), true),
                )
            }
        };

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
                Decision::Allow => true,
                Decision::Ask => approver.approve(&request).await,
                Decision::Deny => false,
            };
            if !allowed {
                return ToolResult::error(
                    call.id.clone(),
                    format_tool_output(
                        tool.name(),
                        &format!("permission denied for {}", tool.name()),
                        true,
                    ),
                );
            }
        }

        for gate in gates {
            if let GateVerdict::Block { reason } = gate.check(call, &effects) {
                return ToolResult::error(
                    call.id.clone(),
                    format_tool_output(
                        tool.name(),
                        &format!("blocked by {}: {reason}", gate.name()),
                        true,
                    ),
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
                    output: format_tool_output(tool.name(), &bounded, output.is_error),
                    is_error: output.is_error,
                }
            }
            Err(err) => ToolResult::error(
                call.id.clone(),
                format_tool_output(tool.name(), &err.to_string(), true),
            ),
        }
    }
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

fn format_tool_output(tool: &str, output: &str, is_error: bool) -> String {
    let status = if is_error { "error" } else { "success" };
    format!("tool: {tool}\nstatus: {status}\noutput:\n{output}")
}
