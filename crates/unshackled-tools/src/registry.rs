//! The tool registry and permission-gated dispatch.

use serde_json::Value;
use unshackled_config::redact::redact;
use unshackled_core::{ToolCall, ToolResult};
use unshackled_sandbox::{Approver, Decision, PermissionEngine, PermissionRequest};

use crate::builtins::{
    EditFile, GitCommit, GitStatus, ListFiles, ReadFile, RunShell, SearchText, WriteFile,
};
use crate::tool::{Tool, ToolContext};

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

    /// A registry with all eight builtin tools.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        registry.register(Box::new(ReadFile));
        registry.register(Box::new(WriteFile));
        registry.register(Box::new(EditFile));
        registry.register(Box::new(ListFiles));
        registry.register(Box::new(SearchText));
        registry.register(Box::new(RunShell));
        registry.register(Box::new(GitStatus));
        registry.register(Box::new(GitCommit));
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
    pub fn names(&self) -> Vec<&'static str> {
        self.tools.iter().map(|t| t.name()).collect()
    }

    /// The registered tools' name + JSON schema pairs.
    #[must_use]
    pub fn schemas(&self) -> Vec<(&'static str, Value)> {
        self.tools.iter().map(|t| (t.name(), t.schema())).collect()
    }

    /// The registered tools' name, description, and JSON schema, for building
    /// provider tool specifications.
    #[must_use]
    pub fn specs(&self) -> Vec<(&'static str, &'static str, Value)> {
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
        let Some(tool) = self.get(&call.name) else {
            return ToolResult::error(call.id.clone(), format!("unknown tool: {}", call.name));
        };

        let effects = match tool.effects(&call.input, ctx) {
            Ok(effects) => effects,
            Err(err) => return ToolResult::error(call.id.clone(), err.to_string()),
        };

        let detail = target_detail(&call.input);
        for effect in &effects {
            let request = PermissionRequest {
                tool: tool.name(),
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
                    format!("permission denied for {}", tool.name()),
                );
            }
        }

        match tool.invoke(call.input.clone(), ctx).await {
            // Redaction happens here, for every profile including bypass.
            Ok(output) => ToolResult {
                id: call.id.clone(),
                output: redact(&output.text),
                is_error: output.is_error,
            },
            Err(err) => ToolResult::error(call.id.clone(), err.to_string()),
        }
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// A short description of a tool call's concrete target, drawn from the common
/// input fields, for an approval prompt. Returns an empty string when none is
/// present. The value is not trusted for any decision — it is display only.
fn target_detail(input: &Value) -> String {
    for key in ["command", "path", "url", "pattern", "query"] {
        if let Some(value) = input.get(key).and_then(Value::as_str) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                let mut shown: String = trimmed.chars().take(120).collect();
                if trimmed.chars().count() > 120 {
                    shown.push('…');
                }
                return shown;
            }
        }
    }
    String::new()
}
