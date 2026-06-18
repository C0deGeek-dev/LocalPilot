//! A live, fingerprinted projection of the tool registry: the searchable surface
//! the pull-discovery broker resolves needs against (ADR-0031).
//!
//! The catalog is *derived and disposable* — a projection over the registry
//! (builtins + dynamically registered MCP tools), rebuilt on the registry-change
//! signal (registration / MCP (re)connect), never a second source of truth. Each
//! entry carries a content **fingerprint**: a stable FNV-1a hash of (name +
//! description + schema + source). Identical metadata yields an identical
//! fingerprint; any field change yields a different one, so adds, removals, and
//! schema bumps produce an index [`CatalogDelta`] with no manual upkeep —
//! **change-aware invalidation** without polling or a filesystem walk.
//!
//! MCP is the volatile edge: a server's advertised tool list is authoritative for
//! its entries on each enumeration, so a tool a server no longer advertises is
//! simply absent from the next projection and shows up as a `removed` delta. MCP
//! carries no deprecation field (the protocol exposes only name/title/description/
//! schema/annotations), so deprecation is an [`DeprecationOverlay`] only — it
//! annotates and de-ranks an entry; it grants and removes nothing.

use std::collections::BTreeMap;

use serde_json::Value;

/// Where a catalog entry's tool comes from. The source discriminates entries by
/// provenance — a builtin vs a specific MCP server — and feeds the fingerprint, so
/// the same tool name served by a different source fingerprints differently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSource {
    /// A builtin tool compiled into LocalPilot.
    Builtin,
    /// A tool advertised by the named MCP server.
    Mcp(String),
}

impl ToolSource {
    /// The stable label folded into the fingerprint. MCP carries no per-tool
    /// version, so the server id is the provenance discriminator.
    fn label(&self) -> &str {
        match self {
            ToolSource::Builtin => "builtin",
            ToolSource::Mcp(server) => server,
        }
    }
}

/// FNV-1a 64-bit offset basis. Pinned in-crate: a content fingerprint must be
/// stable across runs and toolchains, which `std::hash::DefaultHasher` does not
/// promise. FNV-1a is deterministic by construction and needs no dependency.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Fold one byte into a running FNV-1a state.
#[inline]
fn fnv_step(hash: u64, byte: u8) -> u64 {
    (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
}

/// Fold a length-prefixed field into a running FNV-1a state. The length prefix
/// makes field boundaries unambiguous, so `("ab", "c")` and `("a", "bc")` cannot
/// collide regardless of the bytes inside a field.
fn fnv_field(mut hash: u64, field: &[u8]) -> u64 {
    for byte in (field.len() as u64).to_le_bytes() {
        hash = fnv_step(hash, byte);
    }
    for &byte in field {
        hash = fnv_step(hash, byte);
    }
    hash
}

/// The content fingerprint of a tool's metadata: a stable FNV-1a hash over name,
/// description, canonical-JSON schema, and source. `serde_json::Value` maps are
/// `BTreeMap`-backed here (no `preserve_order`), so `Value::to_string()` is
/// canonical (sorted keys) and stable for hashing.
#[must_use]
pub fn fingerprint(name: &str, description: &str, schema: &Value, source: &ToolSource) -> u64 {
    let schema_text = schema.to_string();
    let mut hash = FNV_OFFSET;
    hash = fnv_field(hash, name.as_bytes());
    hash = fnv_field(hash, description.as_bytes());
    hash = fnv_field(hash, schema_text.as_bytes());
    hash = fnv_field(hash, source.label().as_bytes());
    hash
}

/// One tool projected into the searchable surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    pub name: String,
    pub description: String,
    pub schema: Value,
    pub source: ToolSource,
    /// The content fingerprint (see [`fingerprint`]).
    pub fingerprint: u64,
}

/// A live projection of the registry. Holds no tool the registry does not; it is
/// rebuilt from the registry, never edited in place to diverge from it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    entries: Vec<CatalogEntry>,
}

/// What changed between two catalog projections. `changed` is a name present in
/// both with a different fingerprint (a schema/description bump).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogDelta {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
}

impl CatalogDelta {
    /// Whether nothing changed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

impl Catalog {
    /// Project a catalog from `(name, description, schema, source)` items.
    pub fn project<I, N, D>(items: I) -> Self
    where
        I: IntoIterator<Item = (N, D, Value, ToolSource)>,
        N: Into<String>,
        D: Into<String>,
    {
        let entries = items
            .into_iter()
            .map(|(name, description, schema, source)| {
                let name = name.into();
                let description = description.into();
                let fingerprint = fingerprint(&name, &description, &schema, &source);
                CatalogEntry {
                    name,
                    description,
                    schema,
                    source,
                    fingerprint,
                }
            })
            .collect();
        Self { entries }
    }

    /// The projected entries.
    #[must_use]
    pub fn entries(&self) -> &[CatalogEntry] {
        &self.entries
    }

    /// The number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the catalog is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Look up one entry by exact name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&CatalogEntry> {
        self.entries.iter().find(|entry| entry.name == name)
    }

    /// The delta from `self` (old) to `next` (new). Names only in `next` are
    /// `added`, names only in `self` are `removed`, names in both with a different
    /// fingerprint are `changed`. Output names are sorted for a stable order.
    #[must_use]
    pub fn delta(&self, next: &Catalog) -> CatalogDelta {
        let old: BTreeMap<&str, u64> = self
            .entries
            .iter()
            .map(|e| (e.name.as_str(), e.fingerprint))
            .collect();
        let new: BTreeMap<&str, u64> = next
            .entries
            .iter()
            .map(|e| (e.name.as_str(), e.fingerprint))
            .collect();
        let mut delta = CatalogDelta::default();
        for (name, fingerprint) in &new {
            match old.get(name) {
                None => delta.added.push((*name).to_string()),
                Some(old_fp) if old_fp != fingerprint => delta.changed.push((*name).to_string()),
                Some(_) => {}
            }
        }
        for name in old.keys() {
            if !new.contains_key(name) {
                delta.removed.push((*name).to_string());
            }
        }
        delta
    }

    /// Change-aware reprojection: build the next catalog from `items`, reusing
    /// this catalog's entries whose fingerprint is unchanged so only added and
    /// changed entries are rebuilt, and return the next catalog plus the delta
    /// (the invalidation signal a dependent cache acts on).
    pub fn reproject<I, N, D>(&self, items: I) -> (Catalog, CatalogDelta)
    where
        I: IntoIterator<Item = (N, D, Value, ToolSource)>,
        N: Into<String>,
        D: Into<String>,
    {
        let fresh = Catalog::project(items);
        let delta = self.delta(&fresh);
        // Reuse the unchanged entries verbatim (same fingerprint ⇒ identical
        // metadata), rebuilding only what the delta marks added or changed.
        let reused: Vec<CatalogEntry> = fresh
            .entries
            .into_iter()
            .map(|entry| match self.get(&entry.name) {
                Some(prev) if prev.fingerprint == entry.fingerprint => prev.clone(),
                _ => entry,
            })
            .collect();
        (Catalog { entries: reused }, delta)
    }
}

/// An optional, hand-maintained old→replacement map for tools an MCP server has
/// retired. MCP carries no deprecation field, so this overlay is the only way to
/// sharpen a "X retired; use Y" hint. It **annotates and de-ranks**; it grants and
/// removes nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeprecationOverlay {
    replacements: BTreeMap<String, Option<String>>,
}

impl DeprecationOverlay {
    /// An empty overlay.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark `old` deprecated in favour of `replacement` (the overlay path).
    pub fn deprecate(&mut self, old: impl Into<String>, replacement: impl Into<String>) {
        self.replacements
            .insert(old.into(), Some(replacement.into()));
    }

    /// Mark `old` deprecated with no known replacement.
    pub fn retire(&mut self, old: impl Into<String>) {
        self.replacements.insert(old.into(), None);
    }

    /// Best-effort read of an MCP descriptor's free-form `_meta` for a deprecation
    /// hint. This is *off* the MCP standard (the protocol carries no deprecation
    /// field), so it is a no-op unless a server volunteers `_meta.deprecated` and,
    /// optionally, `_meta.replacedBy`. The overlay path above is the supported one.
    pub fn note_from_meta(&mut self, name: &str, meta: &Value) {
        let deprecated = meta
            .get("deprecated")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !deprecated {
            return;
        }
        match meta.get("replacedBy").and_then(Value::as_str) {
            Some(replacement) => self.deprecate(name, replacement),
            None => self.retire(name),
        }
    }

    /// Whether `name` is marked deprecated.
    #[must_use]
    pub fn is_deprecated(&self, name: &str) -> bool {
        self.replacements.contains_key(name)
    }

    /// The known replacement for `name`, if the overlay records one.
    #[must_use]
    pub fn replacement_for(&self, name: &str) -> Option<&str> {
        self.replacements.get(name).and_then(Option::as_deref)
    }

    /// Whether the overlay records nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.replacements.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema(kind: &str) -> Value {
        json!({ "type": "object", "properties": { "x": { "type": kind } } })
    }

    // --- fingerprint (02.1) ---

    #[test]
    fn identical_metadata_yields_an_identical_fingerprint() {
        let a = fingerprint(
            "read",
            "reads a file",
            &schema("string"),
            &ToolSource::Builtin,
        );
        let b = fingerprint(
            "read",
            "reads a file",
            &schema("string"),
            &ToolSource::Builtin,
        );
        assert_eq!(a, b);
    }

    #[test]
    fn any_field_change_changes_the_fingerprint() {
        let base = fingerprint(
            "read",
            "reads a file",
            &schema("string"),
            &ToolSource::Builtin,
        );
        // name
        assert_ne!(
            base,
            fingerprint(
                "write",
                "reads a file",
                &schema("string"),
                &ToolSource::Builtin
            )
        );
        // description
        assert_ne!(
            base,
            fingerprint(
                "read",
                "reads bytes",
                &schema("string"),
                &ToolSource::Builtin
            )
        );
        // schema
        assert_ne!(
            base,
            fingerprint(
                "read",
                "reads a file",
                &schema("number"),
                &ToolSource::Builtin
            )
        );
        // source
        assert_ne!(
            base,
            fingerprint(
                "read",
                "reads a file",
                &schema("string"),
                &ToolSource::Mcp("files".to_string())
            )
        );
    }

    #[test]
    fn field_boundaries_do_not_collide() {
        // Length-prefixing means concatenation ambiguity cannot collide.
        let a = fingerprint("ab", "c", &Value::Null, &ToolSource::Builtin);
        let b = fingerprint("a", "bc", &Value::Null, &ToolSource::Builtin);
        assert_ne!(a, b);
    }

    // --- projection + lookup (02.2) ---

    #[test]
    fn project_builds_one_entry_per_tool_with_a_fingerprint() {
        let catalog = Catalog::project([
            (
                "read",
                "reads a file",
                schema("string"),
                ToolSource::Builtin,
            ),
            (
                "fetch",
                "gets a url",
                schema("string"),
                ToolSource::Mcp("net".to_string()),
            ),
        ]);
        assert_eq!(catalog.len(), 2);
        let read = catalog.get("read").expect("read entry");
        assert_eq!(
            read.fingerprint,
            fingerprint(
                "read",
                "reads a file",
                &schema("string"),
                &ToolSource::Builtin
            )
        );
        assert!(catalog.get("missing").is_none());
    }

    // --- delta: adds / removes / changes (02.2, 02.3, 02.4) ---

    #[test]
    fn delta_reports_added_removed_and_changed() {
        let old = Catalog::project([
            ("keep", "unchanged", schema("string"), ToolSource::Builtin),
            ("drop", "going away", schema("string"), ToolSource::Builtin),
            ("bump", "v1", schema("string"), ToolSource::Builtin),
        ]);
        let new = Catalog::project([
            ("keep", "unchanged", schema("string"), ToolSource::Builtin),
            ("bump", "v2", schema("string"), ToolSource::Builtin), // description bump
            ("fresh", "brand new", schema("string"), ToolSource::Builtin),
        ]);
        let delta = old.delta(&new);
        assert_eq!(delta.added, vec!["fresh".to_string()]);
        assert_eq!(delta.removed, vec!["drop".to_string()]);
        assert_eq!(delta.changed, vec!["bump".to_string()]);
        assert!(!delta.is_empty());
    }

    #[test]
    fn an_identical_reprojection_is_an_empty_delta() {
        let entries = || {
            [
                ("a", "alpha", schema("string"), ToolSource::Builtin),
                ("b", "beta", schema("string"), ToolSource::Builtin),
            ]
        };
        let old = Catalog::project(entries());
        let (next, delta) = old.reproject(entries());
        assert!(delta.is_empty(), "no metadata changed");
        assert_eq!(old, next, "reprojection of identical metadata is identical");
    }

    #[test]
    fn a_fake_mcp_source_that_adds_renames_and_drops_is_tracked() {
        // First enumeration of server "files".
        let first = Catalog::project([
            (
                "read_doc",
                "read a document",
                schema("string"),
                ToolSource::Mcp("files".to_string()),
            ),
            (
                "old_name",
                "legacy tool",
                schema("string"),
                ToolSource::Mcp("files".to_string()),
            ),
        ]);
        // Second enumeration: old_name renamed to new_name (rename = drop + add),
        // read_doc dropped, write_doc added.
        let (second, delta) = first.reproject([
            (
                "new_name",
                "renamed tool",
                schema("string"),
                ToolSource::Mcp("files".to_string()),
            ),
            (
                "write_doc",
                "write a document",
                schema("string"),
                ToolSource::Mcp("files".to_string()),
            ),
        ]);
        assert_eq!(
            delta.added,
            vec!["new_name".to_string(), "write_doc".to_string()]
        );
        assert_eq!(
            delta.removed,
            vec!["old_name".to_string(), "read_doc".to_string()]
        );
        assert!(delta.changed.is_empty());
        // The dropped tools are gone from the catalog; the new ones are present.
        assert!(second.get("old_name").is_none());
        assert!(second.get("read_doc").is_none());
        assert!(second.get("new_name").is_some());
    }

    // --- deprecation overlay (02.5) ---

    #[test]
    fn overlay_annotates_via_the_hand_maintained_map() {
        let mut overlay = DeprecationOverlay::new();
        overlay.deprecate("old_tool", "new_tool");
        overlay.retire("gone_tool");
        assert!(overlay.is_deprecated("old_tool"));
        assert_eq!(overlay.replacement_for("old_tool"), Some("new_tool"));
        assert!(overlay.is_deprecated("gone_tool"));
        assert_eq!(overlay.replacement_for("gone_tool"), None);
        assert!(!overlay.is_deprecated("live_tool"));
    }

    #[test]
    fn overlay_reads_a_servers_free_form_meta_when_present() {
        let mut overlay = DeprecationOverlay::new();
        // A server that volunteers a non-standard _meta deprecation hint.
        overlay.note_from_meta(
            "legacy",
            &json!({ "deprecated": true, "replacedBy": "modern" }),
        );
        assert_eq!(overlay.replacement_for("legacy"), Some("modern"));
        // No _meta deprecation ⇒ nothing recorded.
        overlay.note_from_meta("fine", &json!({ "deprecated": false }));
        assert!(!overlay.is_deprecated("fine"));
        overlay.note_from_meta("bare", &json!({}));
        assert!(!overlay.is_deprecated("bare"));
    }
}
