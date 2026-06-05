//! `unshackled memory` subcommands over LocalMind accepted memory.

use std::io::Write;
use std::path::Path;

/// Print a one-line status: entry count and whether injection is enabled.
///
/// # Errors
/// Returns an error if the store cannot be read or output written.
pub fn status(root: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let count = unshackled_localmind::memory_list(root)?.len();
    let state = if unshackled_localmind::memory_injection_enabled(root) {
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
    for entry in unshackled_localmind::memory_list(root)? {
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
    for entry in unshackled_localmind::search(root, query)? {
        writeln!(out, "{}  {}", entry.memory_id, entry.snippet)?;
    }
    Ok(())
}

/// Delete an entry by id.
///
/// # Errors
/// Returns an error if the store cannot be written or output written.
pub fn delete(root: &Path, id: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    if unshackled_localmind::memory_delete(root, id)? {
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
    unshackled_localmind::memory_disable_injection(root)?;
    writeln!(out, "memory injection disabled for this project")?;
    Ok(())
}
