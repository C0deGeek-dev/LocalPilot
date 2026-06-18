//! The pull-discovery broker: need→tool resolution, a per-session working set,
//! and reveal-into-visibility (ADR-0031).
//!
//! The broker resolves a *need* (free text, or a failed call's intent) to the
//! best tool(s) in the live [`Catalog`] using a deterministic in-process
//! word-overlap scorer, maintains a bounded per-session **working set**, and
//! **reveals** a resolved tool — adds it to the working set and returns the
//! tool's exact current schema plus a one-line usage example.
//!
//! **Reveal-never-grant.** Reveal mutates *visibility only* — the advertised set
//! the session projects each turn. It never executes a tool and never touches the
//! permission engine or the tighten-only gates; a revealed write/network tool
//! still hits the same `Ask`/`Deny` it would have hit had it always been
//! advertised. The broker's own surface (`tool_search`, `tool_load`) is read-only
//! (`Effect::ReadPath`), mirroring `skill_search`/`skill_load`: searching and
//! revealing inject *content the model reads*; they enable nothing.

use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;
use localpilot_sandbox::Effect;
use parking_lot::Mutex;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::catalog::{Catalog, CatalogEntry, DeprecationOverlay};
use crate::error::ToolError;
use crate::tool::{Tool, ToolContext, ToolOutput};

/// The model-callable name for the search surface.
pub const TOOL_SEARCH: &str = "tool_search";
/// The model-callable name for the reveal surface.
pub const TOOL_LOAD: &str = "tool_load";

/// Ranked locators returned by a search are capped so a turn spends a bounded
/// number of tokens to *find* a tool before paying for any schema. Mirrors
/// `skill_search`'s `MAX_LOCATORS`.
const MAX_LOCATORS: usize = 10;
/// One-line summary length for a locator. Mirrors `skill_search`'s `SUMMARY_CHARS`.
const SUMMARY_CHARS: usize = 100;
/// Default bound on the revealed working set (LRU eviction past it). Evicting a
/// revealed tool only un-advertises it; the model can re-reveal on demand.
pub const DEFAULT_WORKING_SET_CAP: usize = 24;
/// Default minimum resolution score to reveal. At or below it, the broker reports
/// "no tool matches" rather than revealing an irrelevant tool.
pub const DEFAULT_SCORE_FLOOR: u32 = 1;

/// The default core working set: the always-advertised builtins a coding turn
/// needs from turn one. Everything else (git, fetch, knowledge expand/fetch,
/// memory, skills, MCP tools) is revealed on demand. Projects opt in to the
/// broker, so this default only takes effect when narrowing is enabled.
pub const DEFAULT_CORE: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "replace_in_file",
    "apply_patch",
    "list_files",
    "find_files",
    "search_text",
    "run_shell",
    "read_tool_output",
    "knowledge_search",
];

/// Tuning the broker reads. The `enabled` switch lives a level up: the session
/// holds an `Option<Broker>`, so "off" is simply no broker — today's behaviour,
/// the rollback path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerConfig {
    /// Tool names always advertised, in addition to the broker's own tools.
    pub core: Vec<String>,
    /// Maximum revealed tools retained before LRU eviction.
    pub working_set_cap: usize,
    /// Minimum resolution score to reveal.
    pub score_floor: u32,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            core: DEFAULT_CORE.iter().map(|s| (*s).to_string()).collect(),
            working_set_cap: DEFAULT_WORKING_SET_CAP,
            score_floor: DEFAULT_SCORE_FLOOR,
        }
    }
}

/// A ranked match for a need: a tool name, a one-line summary, a score, and any
/// deprecation replacement the overlay records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Locator {
    pub name: String,
    pub summary: String,
    pub score: u32,
    pub deprecated_replacement: Option<String>,
    pub deprecated: bool,
}

/// Collapse text to a single bounded line for a locator summary.
fn one_line(text: &str) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut shown: String = collapsed.chars().take(SUMMARY_CHARS).collect();
    if collapsed.chars().count() > SUMMARY_CHARS {
        shown.push('…');
    }
    shown
}

/// Split a need into matchable lowercase words (length > 2, like `skill_search`).
fn need_words(need_lower: &str) -> Vec<&str> {
    need_lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| w.len() > 2)
        .collect()
}

/// Word-overlap score of a catalog entry against a need: how many need-words the
/// entry's name+description contains, plus a bonus when the need names the tool
/// directly. Mirrors the `skill_search` scorer applied to tools.
fn score_entry(entry: &CatalogEntry, words: &[&str], need_lower: &str) -> u32 {
    let haystack = format!("{} {}", entry.name, entry.description).to_ascii_lowercase();
    let word_hits = words.iter().filter(|w| haystack.contains(**w)).count() as u32;
    let name_lower = entry.name.to_ascii_lowercase();
    let name_bonus = u32::from(
        need_lower.contains(&name_lower) || words.iter().any(|w| name_lower.contains(*w)),
    ) * 2;
    word_hits + name_bonus
}

/// Resolve a need to ranked locators over `catalog`, de-ranking deprecated
/// entries per `overlay`. Pure. Highest score first; among equal scores a
/// non-deprecated tool wins, ties broken by name. Capped to `MAX_LOCATORS`.
#[must_use]
pub fn resolve(catalog: &Catalog, overlay: &DeprecationOverlay, need: &str) -> Vec<Locator> {
    let need_lower = need.to_ascii_lowercase();
    let words = need_words(&need_lower);
    let mut hits: Vec<Locator> = catalog
        .entries()
        .iter()
        .filter_map(|entry| {
            let score = score_entry(entry, &words, &need_lower);
            if score == 0 {
                return None;
            }
            let deprecated = overlay.is_deprecated(&entry.name);
            Some(Locator {
                name: entry.name.clone(),
                summary: one_line(&entry.description),
                score,
                deprecated_replacement: overlay.replacement_for(&entry.name).map(str::to_string),
                deprecated,
            })
        })
        .collect();
    // Highest score first; a non-deprecated tool outranks a deprecated one at the
    // same score; ties broken by name for a stable order.
    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.deprecated.cmp(&b.deprecated))
            .then_with(|| a.name.cmp(&b.name))
    });
    hits.truncate(MAX_LOCATORS);
    hits
}

/// A bounded, per-session set of revealed tool names in LRU order (least-recently
/// revealed first). Not persisted across sessions.
#[derive(Debug, Clone, Default)]
struct WorkingSet {
    revealed: Vec<String>,
    cap: usize,
}

impl WorkingSet {
    fn new(cap: usize) -> Self {
        Self {
            revealed: Vec::new(),
            cap,
        }
    }

    /// Reveal `name`: move it to most-recently-used, evicting the least-recently
    /// used if the cap is exceeded. A `cap` of 0 means unbounded.
    fn reveal(&mut self, name: &str) {
        self.revealed.retain(|n| n != name);
        self.revealed.push(name.to_string());
        if self.cap > 0 && self.revealed.len() > self.cap {
            self.revealed.remove(0);
        }
    }

    fn contains(&self, name: &str) -> bool {
        self.revealed.iter().any(|n| n == name)
    }

    fn names(&self) -> Vec<String> {
        self.revealed.clone()
    }
}

/// The outcome of a reveal request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevealOutcome {
    /// The tool was found and revealed; `rendered` is the model-visible block.
    Revealed { name: String, rendered: String },
    /// No catalog tool has that exact name.
    NotInCatalog,
}

/// The outcome of a failure-driven re-resolution: a model-visible message and, if
/// a tool was revealed, its name (so the caller can record the resolution).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// The tool name revealed (added to the working set), if any.
    pub revealed: Option<String>,
    /// The model-visible text to return in place of a bare `unknown tool` error.
    pub message: String,
    /// The need the resolution ran against (for telemetry).
    pub need: String,
    /// The resolution score of the revealed tool, if any (for telemetry).
    pub score: u32,
}

/// The broker's per-session state: tuning, the live catalog, the revealed working
/// set, and the deprecation overlay.
struct BrokerState {
    config: BrokerConfig,
    catalog: Catalog,
    revealed: WorkingSet,
    overlay: DeprecationOverlay,
}

impl BrokerState {
    fn is_advertised(&self, name: &str) -> bool {
        name == TOOL_SEARCH
            || name == TOOL_LOAD
            || self.config.core.iter().any(|c| c == name)
            || self.revealed.contains(name)
    }

    fn reveal(&mut self, name: &str) -> RevealOutcome {
        match self.catalog.get(name) {
            Some(entry) => {
                let rendered = render_reveal(entry, &self.overlay);
                self.revealed.reveal(name);
                RevealOutcome::Revealed {
                    name: name.to_string(),
                    rendered,
                }
            }
            None => RevealOutcome::NotInCatalog,
        }
    }

    /// Re-resolve an attempted-but-unavailable tool (unknown, out-of-working-set,
    /// or retired) to the closest available tool and reveal it. Never executes
    /// anything; the model retries with the revealed tool. Returns a terminal
    /// "no tool matches" when nothing scores at or above the floor (the
    /// resolve-loop guard).
    fn reresolve(&mut self, attempted: &str) -> Resolution {
        // A known deprecation replacement (the overlay) sharpens a retired-tool
        // hint: "X retired; closest now: Y".
        if let Some(replacement) = self.overlay.replacement_for(attempted).map(str::to_string) {
            if self.catalog.get(&replacement).is_some() {
                if let RevealOutcome::Revealed { name, rendered } = self.reveal(&replacement) {
                    return Resolution {
                        message: format!(
                            "tool `{attempted}` is retired; closest available now: `{name}`.\n\
                             {rendered}\nNow advertised — retry with `{name}`.",
                        ),
                        revealed: Some(name),
                        need: attempted.to_string(),
                        score: u32::MAX, // an explicit overlay hit is the strongest signal
                    };
                }
            }
        }

        let hits = resolve(&self.catalog, &self.overlay, attempted);
        match hits.into_iter().next() {
            Some(top) if top.score >= self.config.score_floor => {
                let score = top.score;
                let message = match self.reveal(&top.name) {
                    RevealOutcome::Revealed { name, rendered } if name == attempted => format!(
                        "tool `{attempted}` was not advertised; it is now revealed.\n\
                         {rendered}\nRetry the call.",
                    ),
                    RevealOutcome::Revealed { name, rendered } => format!(
                        "tool `{attempted}` is not available; closest available: `{name}`.\n\
                         {rendered}\nNow advertised — retry with `{name}`.",
                    ),
                    RevealOutcome::NotInCatalog => {
                        format!("no available tool matches `{attempted}`.",)
                    }
                };
                Resolution {
                    revealed: Some(top.name),
                    message,
                    need: attempted.to_string(),
                    score,
                }
            }
            _ => Resolution {
                revealed: None,
                message: format!(
                    "no available tool matches `{attempted}`. Describe the capability differently \
                     and call `tool_search`, or proceed without it.",
                ),
                need: attempted.to_string(),
                score: 0,
            },
        }
    }
}

/// A one-line skeletal example call for a tool, built from its input schema's
/// required properties (or the first few when none are required).
fn example_for(name: &str, schema: &Value) -> String {
    let props = schema.get("properties").and_then(Value::as_object);
    let required: Vec<String> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let keys: Vec<String> = if required.is_empty() {
        props
            .map(|p| p.keys().take(3).cloned().collect())
            .unwrap_or_default()
    } else {
        required
    };
    let args = keys
        .iter()
        .map(|key| {
            let ty = props
                .and_then(|p| p.get(key))
                .and_then(|field| field.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("value");
            format!("\"{key}\": <{ty}>")
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("{name}({{{args}}})")
}

/// Render a revealed tool: a no-grant header, its description, a one-line example,
/// the exact schema, and any deprecation replacement hint.
fn render_reveal(entry: &CatalogEntry, overlay: &DeprecationOverlay) -> String {
    let mut out = format!(
        "Revealed `{}` (now advertised — call it normally; revealing runs nothing and grants \
         nothing, so any action still goes through the permission gate):\n",
        entry.name
    );
    let _ = writeln!(out, "description: {}", entry.description);
    let _ = writeln!(out, "example: {}", example_for(&entry.name, &entry.schema));
    if let Some(replacement) = overlay.replacement_for(&entry.name) {
        let _ = writeln!(
            out,
            "note: `{}` is deprecated; prefer `{replacement}`",
            entry.name
        );
    }
    let _ = write!(out, "schema: {}", entry.schema);
    out
}

/// A cloneable handle to the broker's per-session state, shared by the
/// model-callable tools and the session (which reads the advertised set and drives
/// the triggers). Cloning shares the same state.
#[derive(Clone)]
pub struct Broker(Arc<Mutex<BrokerState>>);

impl Broker {
    /// Build a broker with an empty catalog; call [`Broker::set_catalog`] (or
    /// [`Broker::reproject`]) once the registry is built.
    #[must_use]
    pub fn new(config: BrokerConfig) -> Self {
        let cap = config.working_set_cap;
        Self(Arc::new(Mutex::new(BrokerState {
            config,
            catalog: Catalog::default(),
            revealed: WorkingSet::new(cap),
            overlay: DeprecationOverlay::new(),
        })))
    }

    /// Replace the live catalog (e.g. after the registry is built).
    pub fn set_catalog(&self, catalog: Catalog) {
        self.0.lock().catalog = catalog;
    }

    /// Change-aware refresh: reproject the catalog against `fresh`, keeping
    /// unchanged entries, and return the delta (the invalidation signal).
    pub fn reproject(&self, fresh: Catalog) -> crate::catalog::CatalogDelta {
        let mut state = self.0.lock();
        let items = fresh.entries().iter().map(|e| {
            (
                e.name.clone(),
                e.description.clone(),
                e.schema.clone(),
                e.source.clone(),
            )
        });
        let (next, delta) = state.catalog.reproject(items);
        state.catalog = next;
        delta
    }

    /// Record a deprecation in the overlay (old → replacement).
    pub fn deprecate(&self, old: impl Into<String>, replacement: impl Into<String>) {
        self.0.lock().overlay.deprecate(old, replacement);
    }

    /// Whether `name` is in the advertised set this turn (core ∪ broker tools ∪
    /// revealed). The session's advertise lever calls this to narrow `tool_specs`.
    #[must_use]
    pub fn is_advertised(&self, name: &str) -> bool {
        self.0.lock().is_advertised(name)
    }

    /// Resolve a need to ranked locators over the current catalog.
    #[must_use]
    pub fn resolve(&self, need: &str) -> Vec<Locator> {
        let state = self.0.lock();
        resolve(&state.catalog, &state.overlay, need)
    }

    /// Reveal a tool by exact name, adding it to the working set.
    pub fn reveal(&self, name: &str) -> RevealOutcome {
        self.0.lock().reveal(name)
    }

    /// Re-resolve an attempted-but-unavailable tool (the failure-driven trigger):
    /// reveal the closest available tool and return the model-visible message, or a
    /// terminal "no tool matches" when nothing scores at or above the floor.
    pub fn reresolve(&self, attempted: &str) -> Resolution {
        self.0.lock().reresolve(attempted)
    }

    /// The revealed tool names, most-recently-revealed last (for tests/inspection).
    #[must_use]
    pub fn revealed_names(&self) -> Vec<String> {
        self.0.lock().revealed.names()
    }

    /// The configured score floor.
    #[must_use]
    pub fn score_floor(&self) -> u32 {
        self.0.lock().config.score_floor
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ToolSearchInput {
    /// The capability you need but do not have advertised; matched against the
    /// catalog of available tools.
    need: String,
}

/// `tool_search`: find available tools relevant to a need, returning lean ranked
/// locators (tool name, one-line summary, score) — no schemas. Read-only.
pub struct ToolSearch {
    broker: Broker,
}

impl ToolSearch {
    /// Build the search tool over a broker handle.
    #[must_use]
    pub fn new(broker: Broker) -> Self {
        Self { broker }
    }
}

#[async_trait]
impl Tool for ToolSearch {
    fn name(&self) -> &str {
        TOOL_SEARCH
    }

    fn description(&self) -> &str {
        "Search the available tools for ones relevant to a capability you need but do not have \
         advertised, returning a short ranked list of locators (tool name, one-line summary, score) \
         — no schemas. This is the pull-based way to discover tools on demand instead of carrying \
         every tool in context. Then call `tool_load` with a name to reveal that tool's schema. \
         Read-only: searching reveals nothing and runs nothing."
    }

    fn schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(ToolSearchInput)).unwrap_or(Value::Null)
    }

    fn approval_detail(&self, input: &Value) -> String {
        input
            .get("need")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(160)
            .collect()
    }

    fn effects(&self, _input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        Ok(vec![Effect::ReadPath {
            inside_workspace: true,
            secret_like: false,
        }])
    }

    async fn invoke(&self, input: Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: ToolSearchInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        let hits = self.broker.resolve(&input.need);
        if hits.is_empty() {
            return Ok(ToolOutput::ok(format!(
                "no available tool matches \"{}\"",
                input.need
            )));
        }
        let mut out = String::from(
            "Matching tools (locators only — call `tool_load` with a name to reveal its schema):\n",
        );
        for hit in &hits {
            let _ = write!(out, "- {} (score {}): {}", hit.name, hit.score, hit.summary);
            if let Some(replacement) = &hit.deprecated_replacement {
                let _ = write!(out, " [deprecated; prefer `{replacement}`]");
            } else if hit.deprecated {
                let _ = write!(out, " [deprecated]");
            }
            out.push('\n');
        }
        Ok(ToolOutput::ok(out))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ToolLoadInput {
    /// The exact name of the tool to reveal (from `tool_search`).
    name: String,
}

/// `tool_load`: reveal one tool by exact name — add it to this session's working
/// set and return its schema + a one-line example. Read-only: revealing changes
/// visibility only; any action the tool performs still goes through the permission
/// gate.
pub struct ToolLoad {
    broker: Broker,
}

impl ToolLoad {
    /// Build the reveal tool over a broker handle.
    #[must_use]
    pub fn new(broker: Broker) -> Self {
        Self { broker }
    }
}

#[async_trait]
impl Tool for ToolLoad {
    fn name(&self) -> &str {
        TOOL_LOAD
    }

    fn description(&self) -> &str {
        "Reveal one tool by its exact name (from `tool_search`): add it to this session's working \
         set and read back its schema and a one-line example, so you can then call it. Revealing a \
         tool changes only what is advertised — it runs nothing and grants nothing, so any action \
         the tool performs still goes through the normal permission gate."
    }

    fn schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(ToolLoadInput)).unwrap_or(Value::Null)
    }

    fn approval_detail(&self, input: &Value) -> String {
        input
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(160)
            .collect()
    }

    fn effects(&self, _input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        // Revealing is a visibility change and nothing more — never a permission
        // side channel, exactly like loading a skill.
        Ok(vec![Effect::ReadPath {
            inside_workspace: true,
            secret_like: false,
        }])
    }

    async fn invoke(&self, input: Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: ToolLoadInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        match self.broker.reveal(input.name.trim()) {
            RevealOutcome::Revealed { rendered, .. } => Ok(ToolOutput::ok(rendered)),
            RevealOutcome::NotInCatalog => Ok(ToolOutput::ok(format!(
                "no available tool named \"{}\" — call `tool_search` to find one",
                input.name.trim()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::catalog::ToolSource;
    use crate::ToolRegistry;
    use localpilot_core::{ToolCall, ToolUseId};
    use localpilot_sandbox::{
        Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace,
    };
    use serde_json::json;

    fn schema(required: &[&str]) -> Value {
        json!({
            "type": "object",
            "properties": { "path": { "type": "string" }, "url": { "type": "string" } },
            "required": required,
        })
    }

    fn catalog() -> Catalog {
        Catalog::project([
            (
                "fetch",
                "retrieve the body of an http/https url over the network",
                schema(&["url"]),
                ToolSource::Builtin,
            ),
            (
                "git_commit",
                "create a git commit for the staged changes",
                schema(&[]),
                ToolSource::Builtin,
            ),
            (
                "read_file",
                "read utf-8 text from a workspace path",
                schema(&["path"]),
                ToolSource::Builtin,
            ),
        ])
    }

    fn broker() -> Broker {
        let broker = Broker::new(BrokerConfig::default());
        broker.set_catalog(catalog());
        broker
    }

    // --- resolution (03.1) ---

    #[test]
    fn resolve_ranks_the_relevant_tool_first() {
        let broker = broker();
        let hits = broker.resolve("download a file from a url over the network");
        assert!(!hits.is_empty());
        assert_eq!(hits[0].name, "fetch", "got {hits:?}");
    }

    #[test]
    fn resolve_returns_nothing_for_an_unrelated_need() {
        let broker = broker();
        assert!(broker.resolve("xyzzy plugh frobnicate").is_empty());
    }

    #[test]
    fn resolve_deranks_a_deprecated_entry_at_equal_score() {
        // Two entries match equally; the deprecated one sorts second.
        let cat = Catalog::project([
            (
                "old_fetch",
                "fetch a url",
                schema(&["url"]),
                ToolSource::Builtin,
            ),
            (
                "new_fetch",
                "fetch a url",
                schema(&["url"]),
                ToolSource::Builtin,
            ),
        ]);
        let broker = Broker::new(BrokerConfig::default());
        broker.set_catalog(cat);
        broker.deprecate("old_fetch", "new_fetch");
        let hits = broker.resolve("fetch a url");
        assert_eq!(hits[0].name, "new_fetch", "deprecated entry should de-rank");
        assert_eq!(hits[1].deprecated_replacement.as_deref(), Some("new_fetch"));
    }

    // --- working set + reveal (03.2, 03.3) ---

    #[test]
    fn reveal_adds_to_the_working_set_and_returns_schema_and_example() {
        let broker = broker();
        assert!(
            !broker.is_advertised("fetch"),
            "not advertised before reveal"
        );
        let outcome = broker.reveal("fetch");
        match outcome {
            RevealOutcome::Revealed { name, rendered } => {
                assert_eq!(name, "fetch");
                assert!(rendered.contains("schema:"), "got {rendered}");
                assert!(rendered.contains("fetch({"), "example missing: {rendered}");
                assert!(
                    rendered.contains("grants nothing"),
                    "no-grant framing missing: {rendered}"
                );
            }
            RevealOutcome::NotInCatalog => panic!("fetch should be in the catalog"),
        }
        assert!(broker.is_advertised("fetch"), "advertised after reveal");
        assert_eq!(broker.revealed_names(), vec!["fetch".to_string()]);
    }

    #[test]
    fn reveal_of_an_unknown_name_is_a_clean_miss() {
        let broker = broker();
        assert_eq!(broker.reveal("no_such_tool"), RevealOutcome::NotInCatalog);
    }

    #[test]
    fn the_working_set_evicts_least_recently_revealed_past_the_cap() {
        let broker = Broker::new(BrokerConfig {
            core: Vec::new(),
            working_set_cap: 2,
            score_floor: 1,
        });
        broker.set_catalog(Catalog::project([
            ("a", "alpha", schema(&[]), ToolSource::Builtin),
            ("b", "beta", schema(&[]), ToolSource::Builtin),
            ("c", "gamma", schema(&[]), ToolSource::Builtin),
        ]));
        broker.reveal("a");
        broker.reveal("b");
        broker.reveal("c"); // evicts "a" (least recently revealed)
        assert!(!broker.is_advertised("a"), "a should have been evicted");
        assert!(broker.is_advertised("b"));
        assert!(broker.is_advertised("c"));
    }

    // --- failure-driven re-resolution (04.1–04.3) ---

    #[test]
    fn reresolve_reveals_the_closest_tool_and_asks_to_retry() {
        let broker = broker();
        // The model attempted "web_fetch", which does not exist; the closest
        // available tool is "fetch".
        let resolution = broker.reresolve("web_fetch");
        assert_eq!(resolution.revealed.as_deref(), Some("fetch"));
        assert!(
            resolution.message.contains("fetch"),
            "{}",
            resolution.message
        );
        assert!(
            resolution.message.contains("retry"),
            "{}",
            resolution.message
        );
        assert!(
            broker.is_advertised("fetch"),
            "closest tool is now advertised"
        );
    }

    #[test]
    fn reresolve_of_an_out_of_set_tool_by_its_own_name_reveals_it() {
        let broker = broker();
        // "git_commit" exists but is not advertised; resolving its own name
        // reveals it for retry.
        let resolution = broker.reresolve("git_commit");
        assert_eq!(resolution.revealed.as_deref(), Some("git_commit"));
        assert!(broker.is_advertised("git_commit"));
    }

    #[test]
    fn reresolve_routes_a_retired_tool_to_its_replacement() {
        let broker = broker();
        broker.deprecate("legacy_fetch", "fetch");
        let resolution = broker.reresolve("legacy_fetch");
        assert_eq!(resolution.revealed.as_deref(), Some("fetch"));
        assert!(
            resolution.message.contains("retired"),
            "{}",
            resolution.message
        );
        assert!(broker.is_advertised("fetch"));
    }

    #[test]
    fn reresolve_with_no_match_is_terminal() {
        let broker = broker();
        let resolution = broker.reresolve("xyzzy plugh frobnicate");
        assert_eq!(resolution.revealed, None);
        assert!(
            resolution.message.contains("no available tool matches"),
            "{}",
            resolution.message
        );
    }

    // --- advertised set composition (03.4 lever input) ---

    #[test]
    fn core_and_broker_tools_are_always_advertised() {
        let broker = broker();
        assert!(broker.is_advertised("read_file"), "core tool");
        assert!(broker.is_advertised(TOOL_SEARCH), "broker's own search");
        assert!(broker.is_advertised(TOOL_LOAD), "broker's own reveal");
        assert!(!broker.is_advertised("git_commit"), "non-core, unrevealed");
    }

    // --- read-only effect (03.6) ---

    #[tokio::test]
    async fn the_broker_tools_are_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let ctx = ToolContext {
            workspace: &ws,
            interactivity: Interactivity::NonInteractive,
            trusted: true,
            retention: None,
        };
        let read = vec![Effect::ReadPath {
            inside_workspace: true,
            secret_like: false,
        }];
        assert_eq!(
            ToolSearch::new(broker()).effects(&json!({}), &ctx).unwrap(),
            read
        );
        assert_eq!(
            ToolLoad::new(broker()).effects(&json!({}), &ctx).unwrap(),
            read
        );
    }

    // --- reveal-never-grant (03.5, the §7 invariant gate) ---

    #[tokio::test]
    async fn a_revealed_write_tool_still_asks_permission() {
        // Reveal a write tool into the working set, then dispatch a real call to
        // it through the registry. The permission engine must still gate it:
        // revealing changed visibility, not authority.
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let registry = ToolRegistry::with_builtins();

        let broker = Broker::new(BrokerConfig {
            core: Vec::new(),
            working_set_cap: DEFAULT_WORKING_SET_CAP,
            score_floor: 1,
        });
        broker.set_catalog(registry.catalog());
        assert!(
            matches!(broker.reveal("write_file"), RevealOutcome::Revealed { .. }),
            "write_file should be revealable"
        );
        assert!(broker.is_advertised("write_file"), "revealed ⇒ advertised");

        let ctx = ToolContext {
            workspace: &ws,
            interactivity: Interactivity::Interactive,
            trusted: false,
            retention: None,
        };
        let call = ToolCall::new(
            ToolUseId::from("c1"),
            "write_file",
            json!({ "path": "new.txt", "content": "hi" }),
        );
        // A denying approver stands in for the user refusing the prompt: a write
        // under the default profile is `Ask`, so a revealed write tool is denied
        // exactly as a non-revealed one would be — reveal granted nothing.
        let engine = PermissionEngine::new(Profile::Default, Vec::new());
        let result = registry
            .dispatch(&call, &ctx, &engine, &ScriptedApprover::new(vec![false]))
            .await;
        assert!(result.is_error, "got: {}", result.output);
        assert!(
            result.output.contains("permission denied"),
            "revealed write tool bypassed the gate: {}",
            result.output
        );
    }
}
