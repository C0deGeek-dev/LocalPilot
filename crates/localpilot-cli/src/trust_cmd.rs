//! The `localpilot trust` command surface: inspect and manage which workspace
//! folders are trusted, non-interactively and scriptably.
//!
//! Trust is exact-folder — a trusted folder does not extend to its descendants.
//! `status` is informational and exits zero for both trusted and untrusted
//! folders; `add`/`remove`/`list` exit non-zero on a real evaluation or
//! persistence failure (an invalid target, an unavailable config base, or a
//! store read/write error) so an operator can tell a broken store from a
//! genuinely untrusted folder.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::trust::{self, AddOutcome, RemoveOutcome, Trust, TrustError};

fn resolve(path: Option<PathBuf>) -> Result<PathBuf> {
    match path {
        Some(path) => Ok(path),
        None => std::env::current_dir().context("could not determine the current directory"),
    }
}

fn store_path() -> Result<PathBuf> {
    trust::store_path().ok_or_else(|| anyhow!("{}", TrustError::ConfigBaseUnavailable))
}

pub(crate) fn status(path: Option<PathBuf>) -> Result<()> {
    let dir = resolve(path)?;
    let store = store_path()?;
    status_into(&dir, &store, &mut std::io::stdout())
}

pub(crate) fn add(path: Option<PathBuf>) -> Result<()> {
    let dir = resolve(path)?;
    let store = store_path()?;
    add_into(&dir, &store, &mut std::io::stdout())
}

pub(crate) fn remove(path: Option<PathBuf>) -> Result<()> {
    let dir = resolve(path)?;
    let store = store_path()?;
    remove_into(&dir, &store, &mut std::io::stdout())
}

pub(crate) fn list() -> Result<()> {
    let store = store_path()?;
    list_into(&store, &mut std::io::stdout())
}

fn status_into(dir: &Path, store: &Path, out: &mut impl Write) -> Result<()> {
    let folder = trust::canonical_key(dir).map_err(|error| anyhow!("{error}"))?;
    let trust = trust::is_trusted_result_in(dir, store).map_err(|error| anyhow!("{error}"))?;
    let label = match trust {
        Trust::Trusted => "trusted",
        Trust::Untrusted => "not trusted",
    };
    writeln!(out, "workspace trust: {label}")?;
    writeln!(out, "  folder: {folder}")?;
    writeln!(out, "  store:  {}", store.display())?;
    Ok(())
}

fn add_into(dir: &Path, store: &Path, out: &mut impl Write) -> Result<()> {
    let folder = trust::canonical_key(dir).map_err(|error| anyhow!("{error}"))?;
    match trust::add_in(dir, store).map_err(|error| anyhow!("{error}"))? {
        AddOutcome::Added => writeln!(out, "trusted {folder}")?,
        AddOutcome::AlreadyPresent => writeln!(out, "{folder} was already trusted")?,
    }
    writeln!(out, "  store: {}", store.display())?;
    Ok(())
}

fn remove_into(dir: &Path, store: &Path, out: &mut impl Write) -> Result<()> {
    // The evaluated key — the canonical existing-folder path, or the stable
    // absolute fallback for a deleted folder — is the entry actually matched, so
    // report it rather than the raw argument.
    let key = trust::removal_key(dir).map_err(|error| anyhow!("{error}"))?;
    match trust::remove_in(dir, store).map_err(|error| anyhow!("{error}"))? {
        RemoveOutcome::Removed => writeln!(out, "removed trust for {key}")?,
        RemoveOutcome::Absent => writeln!(out, "{key} was not trusted")?,
    }
    writeln!(out, "  store: {}", store.display())?;
    Ok(())
}

fn list_into(store: &Path, out: &mut impl Write) -> Result<()> {
    let entries = trust::list_in(store).map_err(|error| anyhow!("{error}"))?;
    if entries.is_empty() {
        writeln!(out, "no trusted folders")?;
    } else {
        for entry in &entries {
            writeln!(out, "{entry}")?;
        }
    }
    writeln!(out, "  store: {}", store.display())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf8(bytes: Vec<u8>) -> String {
        String::from_utf8(bytes).expect("utf8")
    }

    #[test]
    fn status_reports_both_states_and_add_then_flips_it() {
        let project = tempfile::tempdir().expect("project");
        let home = tempfile::tempdir().expect("home");
        let store = home.path().join("trusted-folders.txt");

        let mut out = Vec::new();
        status_into(project.path(), &store, &mut out).expect("status untrusted");
        assert!(utf8(out).contains("workspace trust: not trusted"));

        let mut out = Vec::new();
        add_into(project.path(), &store, &mut out).expect("add");
        assert!(utf8(out).starts_with("trusted "));

        let mut out = Vec::new();
        status_into(project.path(), &store, &mut out).expect("status trusted");
        assert!(utf8(out).contains("workspace trust: trusted"));

        // Idempotent add.
        let mut out = Vec::new();
        add_into(project.path(), &store, &mut out).expect("re-add");
        assert!(utf8(out).contains("was already trusted"));
    }

    #[test]
    fn remove_reports_absent_then_removed() {
        let project = tempfile::tempdir().expect("project");
        let home = tempfile::tempdir().expect("home");
        let store = home.path().join("trusted-folders.txt");

        let mut out = Vec::new();
        remove_into(project.path(), &store, &mut out).expect("remove absent");
        assert!(utf8(out).contains("was not trusted"));

        add_into(project.path(), &store, &mut Vec::new()).expect("add");
        let mut out = Vec::new();
        remove_into(project.path(), &store, &mut out).expect("remove present");
        assert!(utf8(out).contains("removed trust for"));
    }

    #[test]
    fn remove_prints_the_evaluated_key_for_non_canonical_and_deleted_paths() {
        let project = tempfile::tempdir().expect("project");
        let home = tempfile::tempdir().expect("home");
        let store = home.path().join("trusted-folders.txt");

        // Add the canonical folder, then remove it by a non-canonical spelling
        // (a trailing `.`): the printed key is the evaluated canonical path, not
        // the raw argument.
        let canonical = trust::canonical_key(project.path()).expect("canonical");
        add_into(project.path(), &store, &mut Vec::new()).expect("add");
        let mut out = Vec::new();
        remove_into(&project.path().join("."), &store, &mut out).expect("remove non-canonical");
        let out = utf8(out);
        assert!(
            out.contains(&format!("removed trust for {canonical}")),
            "got {out:?}"
        );

        // A deleted folder is removed via its stable absolute fallback key.
        let ephemeral = tempfile::tempdir().expect("ephemeral");
        let path = ephemeral.path().to_path_buf();
        let absolute = std::path::absolute(&path)
            .expect("absolute")
            .to_string_lossy()
            .into_owned();
        std::fs::write(&store, format!("{absolute}\n")).expect("seed");
        drop(ephemeral);
        let mut out = Vec::new();
        remove_into(&path, &store, &mut out).expect("remove deleted");
        assert!(
            utf8(out).contains(&format!("removed trust for {absolute}")),
            "the stable absolute fallback key must be printed"
        );
    }

    #[test]
    fn list_is_stable_and_status_errors_are_non_zero() {
        let home = tempfile::tempdir().expect("home");
        let store = home.path().join("trusted-folders.txt");
        std::fs::write(&store, "b\na\n").expect("seed");

        let mut out = Vec::new();
        list_into(&store, &mut out).expect("list");
        let text = utf8(out);
        assert!(text.find('a').unwrap() < text.find('b').unwrap());

        // A non-directory target is a hard error (non-zero exit).
        let file = home.path().join("a-file");
        std::fs::write(&file, b"x").expect("write");
        assert!(status_into(&file, &store, &mut Vec::new()).is_err());

        // A real unreadable store (a directory where the file is expected)
        // reaches the command boundary as an error — never "not trusted".
        let project = tempfile::tempdir().expect("project");
        let unreadable = home.path().join("store-as-dir");
        std::fs::create_dir_all(&unreadable).expect("mkdir");
        assert!(status_into(project.path(), &unreadable, &mut Vec::new()).is_err());
    }
}
