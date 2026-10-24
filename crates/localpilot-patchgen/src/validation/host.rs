//! Nonexecuting host program discovery shared by static Git and validation.

use super::ValidationError;
use std::path::{Path, PathBuf};

/// Resolve a simple host executable name through absolute, nonproject PATH
/// locations. Keep its invocation basename while resolving its parent directory;
/// multicall aliases must not silently become another executable mode.
///
/// # Errors
/// Invalid stem or no ordinary host program outside the declared project roots.
pub fn resolve_host_program(name: &str, roots: &[&Path]) -> Result<PathBuf, ValidationError> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
    {
        return Err(ValidationError::Invalid(
            "host executable requires a simple name".into(),
        ));
    }
    let path = std::env::var_os("PATH")
        .ok_or_else(|| ValidationError::Invalid("host PATH is unavailable".into()))?;
    for directory in std::env::split_paths(&path).filter(|p| p.is_absolute()) {
        if let Some(candidate) = trusted_program_at(&directory, name, roots) {
            return Ok(candidate);
        }
    }
    Err(ValidationError::Invalid(
        "registered host executable is unavailable outside the project".into(),
    ))
}

pub(crate) fn trusted_search_directory(directory: &Path, roots: &[&Path]) -> Option<PathBuf> {
    if !directory.is_absolute()
        || directory.ancestors().any(|ancestor| {
            std::fs::canonicalize(ancestor).is_ok_and(|path| roots.iter().any(|root| path == *root))
        })
    {
        return None;
    }
    let parent = std::fs::canonicalize(directory).ok()?;
    if !parent.is_dir() || roots.iter().any(|root| parent.starts_with(root)) {
        return None;
    }
    Some(parent)
}

/// Inspect one host search directory without running or importing its program.
/// Project-owned directory aliases and targets remain untrusted even when their
/// other endpoint is outside the project. The returned path preserves the stem.
#[must_use]
pub fn trusted_program_at(directory: &Path, name: &str, roots: &[&Path]) -> Option<PathBuf> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
    {
        return None;
    }
    let parent = trusted_search_directory(directory, roots)?;
    let name = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.into()
    };
    let candidate = parent.join(name);
    let physical = std::fs::canonicalize(&candidate).ok()?;
    if !physical.is_file() || roots.iter().any(|root| physical.starts_with(root)) {
        return None;
    }
    Some(candidate)
}
