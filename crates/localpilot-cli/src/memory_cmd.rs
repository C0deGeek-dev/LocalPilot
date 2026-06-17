//! `localpilot memory` subcommands over LocalMind accepted memory.

use std::io::Write;
use std::path::Path;

/// Print a one-line status: entry count and whether injection is enabled.
///
/// # Errors
/// Returns an error if the store cannot be read or output written.
pub fn status(root: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let count = localpilot_localmind::memory_list(root)?.len();
    let state = if localpilot_localmind::memory_injection_enabled(root) {
        "enabled"
    } else {
        "disabled"
    };
    writeln!(out, "memory: {count} entries ({state})")?;
    Ok(())
}

/// List all entries (id, kind, text).
///
/// # Errors
/// Returns an error if the store cannot be read or output written.
pub fn inspect(root: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    for entry in localpilot_localmind::memory_list(root)? {
        writeln!(
            out,
            "{}  [{}:{}:{}]  {}",
            entry.id, entry.scope, entry.category, entry.status, entry.body
        )?;
    }
    Ok(())
}

/// List entries relevant to a query.
///
/// # Errors
/// Returns an error if the store cannot be read or output written.
pub fn search(root: &Path, query: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    for entry in localpilot_localmind::search(root, query)? {
        writeln!(out, "{}  {}", entry.memory_id, entry.snippet)?;
    }
    Ok(())
}

/// Render the "memories used this turn" inspector for the latest session: the
/// memories the most recent turn retrieved, each with provenance, confidence,
/// epistemic status, contradictions, and staleness. Fully local.
///
/// # Errors
/// Returns an error if the store cannot be read or output written.
pub fn used(root: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let store = localpilot_store::Store::open(root);
    let Some(entry) = store.latest_session()? else {
        writeln!(out, "No sessions recorded yet.")?;
        return Ok(());
    };
    let events = store.read_events(entry.id)?;
    let used = localpilot_localmind::last_turn_memories_used(&events);
    let inspected = localpilot_localmind::inspect_memories(root, &used)?;
    writeln!(
        out,
        "{}",
        localpilot_localmind::render_inspection(&inspected)
    )?;
    Ok(())
}

/// Delete an entry by id.
///
/// # Errors
/// Returns an error if the store cannot be written or output written.
pub fn delete(root: &Path, id: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    if localpilot_localmind::memory_delete(root, id)? {
        writeln!(out, "deleted {id}")?;
    } else {
        writeln!(out, "no entry with id {id}")?;
    }
    Ok(())
}

/// Disable memory injection for this project.
///
/// # Errors
/// Returns an error if the flag cannot be written.
pub fn disable(root: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    localpilot_localmind::memory_disable_injection(root)?;
    writeln!(out, "memory injection disabled for this project")?;
    Ok(())
}

/// Show a symbol's graph neighborhood, tests, and anchored lessons.
///
/// # Errors
/// Returns an error if the graph cannot be read or output written.
pub fn graph(root: &Path, symbol: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    let report = localpilot_localmind::codegraph_inspect(root, symbol)?;
    writeln!(out, "{}  {}", report.kind, report.qualified_name)?;
    if let Some(path) = &report.path {
        writeln!(out, "  at {path}")?;
    }
    if let Some(skeleton) = &report.skeleton {
        writeln!(out, "  {skeleton}")?;
    }
    if !report.neighbors.is_empty() {
        writeln!(out, "neighbors:")?;
        for neighbor in &report.neighbors {
            writeln!(out, "  {neighbor}")?;
        }
    }
    if !report.tests.is_empty() {
        writeln!(out, "tested by:")?;
        for test in &report.tests {
            writeln!(out, "  {test}")?;
        }
    }
    if !report.knowledge.is_empty() {
        writeln!(out, "lessons:")?;
        for (id, confidence, snippet) in &report.knowledge {
            writeln!(out, "  {id} ({confidence:.2})  {snippet}")?;
        }
    }
    Ok(())
}

/// Write a redacted local snapshot of the code graph.
///
/// # Errors
/// Returns an error if the export fails or output cannot be written.
pub fn export(root: &Path, path: &Path, html: bool, out: &mut dyn Write) -> anyhow::Result<()> {
    let format = if html {
        localpilot_localmind::ExportFormat::Html
    } else {
        localpilot_localmind::ExportFormat::Json
    };
    localpilot_localmind::codegraph_export(root, path, format)?;
    writeln!(out, "graph exported to {}", path.display())?;
    Ok(())
}
