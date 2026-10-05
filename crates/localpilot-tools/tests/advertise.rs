//! A builtin's advertised schema drops only generated annotations, an MCP
//! schema is forwarded untouched, and a broker reveal shows the same schema
//! the next request will advertise.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use async_trait::async_trait;
use localpilot_sandbox::Effect;
use localpilot_tools::{
    advertised_schema, Broker, BrokerConfig, RevealOutcome, Tool, ToolContext, ToolError,
    ToolOutput, ToolRegistry, ToolSource, INTENT_KEY,
};
use serde_json::{json, Value};

/// An MCP-served tool whose schema carries keys a builtin would lose.
struct McpTool;

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        "remote_lookup"
    }
    fn description(&self) -> &str {
        "an MCP tool"
    }
    fn schema(&self) -> Value {
        json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "title": "Lookup",
            "type": "object",
            "x-vendor-rule": { "max_calls": 2 },
            "properties": {
                "n": { "type": "integer", "format": "int32", "default": null }
            }
        })
    }
    fn effects(&self, _input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        Ok(Vec::new())
    }
    async fn invoke(&self, _input: Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::ok("ok"))
    }
}

fn registry_with_mcp() -> ToolRegistry {
    let mut registry = ToolRegistry::with_builtins();
    registry.register_from(Box::new(McpTool), ToolSource::Mcp("remote".to_string()));
    registry
}

fn advertised(registry: &ToolRegistry, name: &str) -> Value {
    registry
        .advertised_specs()
        .into_iter()
        .find(|(tool, _, _)| *tool == name)
        .map(|(_, _, schema)| schema)
        .unwrap()
}

#[test]
fn an_mcp_schema_is_advertised_exactly_as_its_server_sent_it() {
    let registry = registry_with_mcp();
    assert_eq!(advertised(&registry, "remote_lookup"), McpTool.schema());
}

#[test]
fn every_builtin_advertises_its_normalized_schema_and_keeps_the_raw_one_for_repair() {
    let registry = ToolRegistry::with_builtins();
    let raw: Vec<_> = registry.specs();
    let shown = registry.advertised_specs();
    assert_eq!(raw.len(), shown.len());
    let mut intents_kept = false;
    for ((name, description, raw_schema), (shown_name, shown_description, shown_schema)) in
        raw.iter().zip(&shown)
    {
        assert_eq!(name, shown_name);
        assert_eq!(description, shown_description);
        assert_eq!(*shown_schema, advertised_schema(raw_schema), "{name}");
        // The same top-level structure: required fields and property names.
        assert_eq!(
            raw_schema.get("required"),
            shown_schema.get("required"),
            "{name}"
        );
        let keys = |schema: &Value| {
            schema
                .get("properties")
                .and_then(Value::as_object)
                .map(|map| map.keys().cloned().collect::<Vec<_>>())
        };
        assert_eq!(keys(raw_schema), keys(shown_schema), "{name}");
        let raw_text = raw_schema.to_string();
        intents_kept |= raw_text.contains(INTENT_KEY);
        assert!(!shown_schema.to_string().contains(INTENT_KEY), "{name}");
    }
    assert!(
        intents_kept,
        "the raw registry schemas still carry repair intents"
    );
}

#[test]
fn a_revealed_builtin_shows_the_schema_a_request_advertises() {
    let registry = registry_with_mcp();
    let broker = Broker::new(BrokerConfig::default());
    broker.set_catalog(registry.catalog());
    for name in ["read_file", "remote_lookup"] {
        let RevealOutcome::Revealed { rendered, .. } = broker.reveal(name) else {
            panic!("{name} should be in the catalog");
        };
        let schema_text = rendered
            .split_once("schema: ")
            .map(|(_, schema)| schema)
            .unwrap();
        let shown: Value = serde_json::from_str(schema_text).unwrap();
        assert_eq!(shown, advertised(&registry, name), "{name}");
    }
}
