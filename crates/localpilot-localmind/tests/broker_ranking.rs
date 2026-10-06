//! Request-to-tool ranking over the real tool catalog: the builtin registry,
//! the definition search tool, and an MCP server whose tools carry long,
//! similar descriptions. Each row is a user request and the tools the broker's
//! request-driven reveal must (and must not) surface at the shipped floor and
//! cap. Ranking for `tool_search` and failed calls shares the same scorer.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use localpilot_localmind::SearchDefinitions;
use localpilot_tools::{Broker, BrokerConfig, Catalog, ToolRegistry, ToolSource};
use serde_json::json;

const MCP_FILLER: &str = " This tool is part of the project's release-engineering service. It \
    talks to the internal catalogue, respects the project's access policy, returns structured \
    JSON, and never modifies repository files. Results are cached for the length of the session \
    and may lag the catalogue by a few minutes; when a result looks stale, say so instead of \
    retrying in a loop. Inputs are validated server-side and an invalid input returns a \
    descriptive error rather than a partial result.";

const MCP_TOOLS: &[(&str, &str, &str)] = &[
    (
        "release_codename",
        "Look up the internal codename assigned to a release version (for example 2.4.0).",
        "version",
    ),
    (
        "release_owner",
        "Look up which team owns a release train.",
        "train",
    ),
    (
        "release_calendar",
        "List the planned release dates for a quarter.",
        "quarter",
    ),
    (
        "artifact_checksum",
        "Return the published checksum of a release artifact.",
        "artifact",
    ),
    (
        "changelog_entry",
        "Return the curated changelog entry for a version.",
        "version",
    ),
    (
        "support_window",
        "Return the support end date for a version.",
        "version",
    ),
    (
        "mirror_status",
        "Report the sync status of the download mirrors.",
        "",
    ),
    (
        "license_audit",
        "Return the third-party license audit summary for a version.",
        "version",
    ),
];

/// A broker with the shipped defaults over the real catalog.
fn broker() -> Broker {
    let mut registry = ToolRegistry::with_builtins();
    registry.register(Box::new(SearchDefinitions));
    let mut items: Vec<(String, String, serde_json::Value, ToolSource)> = registry
        .catalog()
        .entries()
        .iter()
        .map(|e| {
            (
                e.name.clone(),
                e.description.clone(),
                e.schema.clone(),
                e.source.clone(),
            )
        })
        .collect();
    for (name, description, field) in MCP_TOOLS {
        let schema = if field.is_empty() {
            json!({"type": "object", "properties": {}})
        } else {
            json!({"type": "object", "properties": {*field: {"type": "string"}}})
        };
        items.push((
            (*name).to_string(),
            format!("{description}{MCP_FILLER}"),
            schema,
            ToolSource::Mcp("release".to_string()),
        ));
    }
    let broker = Broker::new(BrokerConfig::default());
    broker.set_catalog(Catalog::project(items));
    broker
}

fn revealed(request: &str) -> Vec<String> {
    broker()
        .reveal_for_request(request)
        .into_iter()
        .filter_map(|r| r.revealed)
        .collect()
}

fn top(need: &str) -> String {
    broker()
        .resolve(need)
        .first()
        .map(|l| l.name.clone())
        .unwrap_or_default()
}

#[test]
fn requests_that_name_a_capability_reveal_its_tool() {
    let rows: &[(&str, &str)] = &[
        ("What is the message of the most recent git commit in this repository? Quote it exactly.", "git_log"),
        ("There is a staged change in this repository. In one sentence, say which file it changes and what it adds.", "git_diff"),
        ("Fetch http://127.0.0.1:8080/status and report the build id it returns.", "fetch"),
        ("First record a two-step task plan with the plan tool, then read README.md and tell me its first line.", "update_plan"),
        ("What is the internal codename of release 2.4.0? Use the release tooling.", "release_codename"),
        ("Find out which team owns the 'stable' release train.", "release_owner"),
        ("Where is the class `RetryPolicy` defined? Give the file path.", "search_definitions"),
    ];
    for (request, expected) in rows {
        let names = revealed(request);
        assert!(
            names.iter().any(|n| n == expected),
            "{request:?} -> {names:?}, expected {expected}"
        );
    }
}

#[test]
fn a_generic_word_in_a_tool_name_does_not_reveal_that_tool() {
    // "commit" asks about history here; git_commit writes commits.
    let names = revealed("Which commit last changed the fragile surcharge, and what was the value before that commit?");
    assert!(!names.iter().any(|n| n == "git_commit"), "{names:?}");
    // "file" alone is not a request for every *_file tool.
    let names = revealed("Where is the class `RetryPolicy` defined? Give the file path.");
    assert!(
        !names
            .iter()
            .any(|n| n == "append_file" || n == "multi_edit"),
        "{names:?}"
    );
    // The shared ranking (tool_search, failed calls) agrees: intent outranks a
    // name that merely contains one of the words.
    assert_ne!(
        top("which commit last changed the surcharge value"),
        "git_commit"
    );
    let ranked: Vec<String> = broker()
        .resolve("where is the class defined file path")
        .into_iter()
        .map(|l| l.name)
        .collect();
    let at = |name: &str| ranked.iter().position(|n| n == name).unwrap_or(usize::MAX);
    assert!(at("search_definitions") < at("read_file"), "{ranked:?}");
}

#[test]
fn history_and_write_intents_rank_their_own_tools() {
    assert_eq!(top("which commit changed this: commit history"), "git_log");
    assert_eq!(top("commit my staged changes with a message"), "git_commit");
}

#[test]
fn an_explicitly_named_tool_is_revealed_and_lookalikes_are_not() {
    let names = revealed("Use git_log to show what happened recently.");
    assert!(names.iter().any(|n| n == "git_log"), "{names:?}");
    // A different identifier that merely contains the name is not a mention.
    for request in [
        "Rename the not_git_log helper.",
        "Configure the git_logger module.",
    ] {
        let names = revealed(request);
        assert!(
            !names.iter().any(|n| n == "git_log"),
            "{request:?} -> {names:?}"
        );
    }
    // Namespaced and repeated mentions resolve to the exact tool.
    assert_eq!(top("release_codename release_codename"), "release_codename");
}

#[test]
fn a_request_without_a_capability_reveals_nothing() {
    for request in [
        "Thanks, that looks right.",
        "Summarise what you did so far in two sentences.",
    ] {
        assert!(revealed(request).is_empty(), "{request:?}");
    }
}

#[test]
fn a_failed_call_by_its_own_name_still_reveals_that_tool() {
    let resolution = broker().reresolve("git_commit");
    assert_eq!(resolution.revealed.as_deref(), Some("git_commit"));
}
