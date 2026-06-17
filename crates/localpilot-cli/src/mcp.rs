//! Loading Model Context Protocol server tools into the session tool registry.
//!
//! Configured servers are launched as local subprocesses. Their tools are
//! registered alongside the builtins and dispatched through the *same*
//! permission engine and redaction — MCP is never a side channel.

use std::sync::Arc;

use localpilot_config::{Config, McpServerConfig};
use localpilot_mcp::{McpClient, McpError, McpTool, McpToolDescriptor, StdioTransport, Transport};
use localpilot_sandbox::Effect;
use localpilot_tools::ToolRegistry;

/// Connected MCP servers and the tools they advertise. The server processes stay
/// alive for as long as this value is held, so a single connection backs many
/// freshly built registries (e.g. one per harness step).
#[derive(Default)]
pub struct McpTools {
    entries: Vec<(McpToolDescriptor, Arc<dyn Transport>)>,
}

impl McpTools {
    /// Spawn every configured MCP server once and discover its tools. A server
    /// that fails to start is skipped with a note on stderr, never aborting.
    pub async fn load(config: &Config) -> Self {
        let mut entries = Vec::new();
        for (name, server) in &config.mcp.servers {
            match connect(server).await {
                Ok(mut discovered) => entries.append(&mut discovered),
                Err(error) => eprintln!("mcp: skipping server '{name}': {error}"),
            }
        }
        Self { entries }
    }

    /// Build a tool registry: the builtins plus every discovered MCP tool. An
    /// MCP tool reaches an external process, so it is gated as a network effect —
    /// the permission engine prompts (or denies) exactly as for a builtin.
    #[must_use]
    pub fn registry(&self) -> ToolRegistry {
        let mut registry = ToolRegistry::with_builtins();
        // The project knowledge base is reachable on demand as a read-only tool,
        // so ingested knowledge is pulled when relevant instead of seeded into
        // every turn. Harmless when no project is ingested (it returns an empty
        // result), and present on every session path that builds a registry.
        registry.register(Box::new(localpilot_localmind::KnowledgeSearch));
        // The expand and fetch layers of the same knowledge base: locate
        // neighbours cheaply, then pay for full bodies only for chosen ids, so a
        // turn spends a bounded number of tokens to find the right context.
        registry.register(Box::new(localpilot_localmind::KnowledgeExpand));
        registry.register(Box::new(localpilot_localmind::KnowledgeFetch));
        // The agent can propose a durable lesson for human review as it works.
        // Enqueue-only — never a direct accepted-memory write.
        registry.register(Box::new(localpilot_localmind::Remember));
        // The agent can read its own LocalMind store directly instead of
        // reverse-engineering SQL: the review queue and accepted memory. Both are
        // read-only — they list/search, never decide, promote, or write.
        registry.register(Box::new(localpilot_localmind::ReviewList));
        registry.register(Box::new(localpilot_localmind::MemorySearch));
        // The agent can surface generated skill drafts (candidate reusable
        // workflows) read-only. Listing a draft never enables it — activation
        // stays a deliberate human step.
        registry.register(Box::new(localpilot_localmind::SkillDrafts));
        // The agent can read active (human-enabled) skills as advisory guidance,
        // read-only. Reading a skill never runs, installs, or changes it.
        registry.register(Box::new(localpilot_localmind::ActiveSkills));
        for (descriptor, transport) in &self.entries {
            registry.register(Box::new(McpTool::new(
                descriptor,
                vec![Effect::Network],
                Arc::clone(transport),
            )));
        }
        registry
    }
}

async fn connect(
    server: &McpServerConfig,
) -> Result<Vec<(McpToolDescriptor, Arc<dyn Transport>)>, McpError> {
    let transport: Arc<dyn Transport> =
        Arc::new(StdioTransport::spawn(&server.command, &server.args)?);
    let client = McpClient::new(Arc::clone(&transport));
    client.initialize().await?;
    let descriptors = client.list_tools().await?;
    Ok(descriptors
        .into_iter()
        .map(|descriptor| (descriptor, Arc::clone(&transport)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_localmind_tools_are_registered_on_every_session_path() {
        // Every session path (REPL, harness, RPC, one-shot) builds its registry
        // through this method, so registering here is registering everywhere.
        let registry = McpTools::default().registry();
        let names = registry.names();
        for expected in [
            "knowledge_search",
            "knowledge_expand",
            "knowledge_fetch",
            "remember",
            "localmind_review_list",
            "localmind_memory_search",
        ] {
            assert!(
                names.contains(&expected),
                "expected `{expected}` in the built registry, got: {names:?}"
            );
        }
    }
}
