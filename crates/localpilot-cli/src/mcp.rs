//! Loading Model Context Protocol server tools into the session tool registry.
//!
//! Configured servers are launched as local subprocesses. Their tools are
//! registered alongside the builtins and dispatched through the *same*
//! permission engine and redaction — MCP is never a side channel.

use std::sync::Arc;

use localpilot_config::{Config, McpServerConfig, ToolsConfig};
use localpilot_mcp::{McpClient, McpError, McpTool, McpToolDescriptor, StdioTransport, Transport};
use localpilot_sandbox::Effect;
use localpilot_tools::{Broker, BrokerConfig, ToolLoad, ToolRegistry, ToolSearch, ToolSource};

/// Connected MCP servers and the tools they advertise. The server processes stay
/// alive for as long as this value is held, so a single connection backs many
/// freshly built registries (e.g. one per harness step).
#[derive(Default)]
pub struct McpTools {
    /// Each entry carries its server id, so the catalog projection can attribute
    /// the tool to that source and a re-enumeration that drops it surfaces as a
    /// catalog `removed` delta.
    entries: Vec<(String, McpToolDescriptor, Arc<dyn Transport>)>,
    /// When set, the model-callable skill discovery tools (`skill_search`,
    /// `skill_load`) are registered so the agent may reach project skills on its
    /// own. Off by default — the deterministic `localpilot skills` surface and the
    /// host-injected user load do not depend on it.
    skills_autonomous: bool,
}

impl McpTools {
    /// Spawn every configured MCP server once and discover its tools. A server
    /// that fails to start is skipped with a note on stderr, never aborting.
    pub async fn load(config: &Config) -> Self {
        let mut entries = Vec::new();
        for (name, server) in &config.mcp.servers {
            match connect(server).await {
                Ok(discovered) => entries.extend(
                    discovered
                        .into_iter()
                        .map(|(descriptor, transport)| (name.clone(), descriptor, transport)),
                ),
                Err(error) => eprintln!("mcp: skipping server '{name}': {error}"),
            }
        }
        Self {
            entries,
            skills_autonomous: config.skills.autonomous_discovery,
        }
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
        // Pull-based project-skill discovery (ADR-0027): the agent can search for
        // and load project-local skills on demand instead of carrying them in
        // context. Registered only when autonomous discovery is enabled, so a
        // small local model never reaches for a skill on its own by default; both
        // tools are read-only and trust-gated regardless.
        if self.skills_autonomous {
            registry.register(Box::new(localpilot_skills::SkillSearch));
            registry.register(Box::new(localpilot_skills::SkillLoad));
        }
        for (server, descriptor, transport) in &self.entries {
            registry.register_from(
                Box::new(McpTool::new(
                    descriptor,
                    vec![Effect::Network],
                    Arc::clone(transport),
                )),
                ToolSource::Mcp(server.clone()),
            );
        }
        registry
    }
}

/// Map the user-facing `[tools]` config into the broker's tuning, falling back to
/// the broker's own defaults for an empty core.
fn broker_config(tools: &ToolsConfig) -> BrokerConfig {
    let defaults = BrokerConfig::default();
    BrokerConfig {
        core: if tools.core.is_empty() {
            defaults.core
        } else {
            tools.core.clone()
        },
        working_set_cap: tools.working_set_cap,
        score_floor: tools.score_floor,
        learning_enabled: tools.learning,
        graduation_threshold: tools.graduation_threshold,
    }
}

/// Build the pull-discovery broker from `[tools]` config when enabled, register
/// its read-only `tool_search`/`tool_load` tools into `registry`, and seed its
/// catalog from the full registry. Returns the broker handle to install on the
/// session via `SessionRuntime::set_broker`, or `None` when the broker is off (the
/// full tool set is advertised as before — the rollback path).
#[must_use]
pub fn install_broker(tools: &ToolsConfig, registry: &mut ToolRegistry) -> Option<Broker> {
    if !tools.broker {
        return None;
    }
    let broker = Broker::new(broker_config(tools));
    registry.register(Box::new(ToolSearch::new(broker.clone())));
    registry.register(Box::new(ToolLoad::new(broker.clone())));
    broker.set_catalog(registry.catalog());
    Some(broker)
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

    #[test]
    fn skill_discovery_tools_are_gated_off_by_default_and_on_when_enabled() {
        // Off by default: the model cannot reach project skills on its own.
        let off = McpTools::default().registry();
        let off_names = off.names();
        assert!(!off_names.contains(&"skill_search"), "got: {off_names:?}");
        assert!(!off_names.contains(&"skill_load"), "got: {off_names:?}");

        // Opted in: both read-only discovery tools are registered.
        let on = McpTools {
            entries: Vec::new(),
            skills_autonomous: true,
        }
        .registry();
        let on_names = on.names();
        assert!(on_names.contains(&"skill_search"), "got: {on_names:?}");
        assert!(on_names.contains(&"skill_load"), "got: {on_names:?}");
    }
}
