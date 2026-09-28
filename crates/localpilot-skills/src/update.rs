//! Crash-safe replacement of installed skills and of a source's cached
//! snapshot (LocalHub#189).
//!
//! Both are multi-step renames, so both keep a durable record of what they are
//! doing before the first rename, and every later manager mutation recovers
//! from it first:
//!
//! - **Package replacement** stages each new copy and sets each old copy
//!   aside in `<scope>/skills-update/`, a sibling of `skills/` and outside
//!   every discovery root, so neither copy is ever a skill candidate. A
//!   journal in phase `swapping` names every step and the prior ledger bytes;
//!   once the new ledger is durable it moves to `committed`. Recovery rolls a
//!   `swapping` journal back and a `committed` one forward.
//! - **A cache swap** writes a marker naming the new commit and the staged
//!   snapshot before it moves the old cache aside, and deletes it only after
//!   the source registry records that commit. The staged snapshot leaves its
//!   place only by becoming the new cache, so recovery rolls back while it is
//!   still staged and forward once it is gone (reconciling the registry).
//!
//! Recovery is idempotent: every step checks what is already done.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::catalog::CatalogPackage;
use crate::error::SkillError;
use crate::install::{copy_package, InstallLedger, Provenance};
use crate::source::SourceRegistry;

/// Where an interrupted step stops, for tests of every recovery path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // each variant is exercised by the tests only
pub(crate) enum Fault {
    /// Saving the new ledger fails.
    LedgerSave,
    /// The swap of the package at this index fails.
    SwapFails(usize),
    /// The process stops after this many packages were swapped.
    StopAfterSwap(usize),
    /// The process stops after the journal is committed, before cleanup.
    StopAfterCommit,
    /// Renaming the new cache into place fails.
    CacheSwap,
    /// The process stops after the swap marker is written, before any rename.
    StopAfterMarker,
    /// Setting the old cache aside fails.
    CacheAsideFails,
    /// The process stops after the old cache is set aside.
    StopAfterCacheAside,
    /// The process stops after the new cache is in place, before the registry
    /// records its commit.
    StopAfterCacheFinal,
}

/// How a transaction ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Done,
    /// A test fault stopped it midway, as a crash would.
    Stopped,
}

fn io(path: &Path, source: std::io::Error) -> SkillError {
    SkillError::Io {
        path: path.display().to_string(),
        source,
    }
}

/// Replace `path` with `bytes` in one rename, so a reader or a crash sees the
/// old file or the new one, never a partial one.
///
/// # Errors
/// [`SkillError::Io`] when the directory, the temporary file or the rename
/// fails; the temporary file is removed.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), SkillError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    std::fs::write(&tmp, bytes).map_err(|e| io(&tmp, e))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        io(path, e)
    })
}

fn remove_dir(path: &Path) -> Result<(), SkillError> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io(path, e)),
    }
}

fn rename(from: &Path, to: &Path) -> Result<(), SkillError> {
    std::fs::rename(from, to).map_err(|e| io(to, e))
}

// --- package replacement ----------------------------------------------------

/// The working directory of an update in a scope.
pub(crate) fn update_dir(base: &Path) -> PathBuf {
    base.join("skills-update")
}

fn journal_path(base: &Path) -> PathBuf {
    update_dir(base).join("journal.toml")
}

#[derive(Debug, Serialize, Deserialize)]
struct Journal {
    /// `swapping` until the new ledger is durable, then `committed`.
    phase: String,
    ledger: PathBuf,
    /// The ledger file's bytes before the update; `None` when it was absent.
    prior_ledger: Option<String>,
    entries: Vec<JournalEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct JournalEntry {
    name: String,
    target: PathBuf,
    aside: PathBuf,
    staging: PathBuf,
    /// Whether an installed copy existed; a missing managed skill has none
    /// to set aside or restore.
    original_exists: bool,
}

/// One package to replace, with the provenance it will have.
pub(crate) struct Change<'a> {
    pub package: &'a CatalogPackage,
    pub provenance: Provenance,
}

fn save_journal(base: &Path, journal: &Journal) -> Result<(), SkillError> {
    let text = toml::to_string_pretty(journal)
        .map_err(|e| SkillError::Corrupt(format!("could not serialize the update journal: {e}")))?;
    write_atomic(&journal_path(base), text.as_bytes())
}

/// Whether an interrupted update in this scope still needs recovery.
pub(crate) fn update_pending(base: &Path) -> bool {
    journal_path(base).exists()
}

/// Replace every package in `changes` as one transaction: all become the new
/// copies with their new ledger entries, or none do.
///
/// # Errors
/// A staging, swap or ledger failure, after the scope was rolled back to its
/// state before the call.
pub(crate) fn replace(
    base: &Path,
    skills_dir: &Path,
    ledger_file: &Path,
    changes: &[Change<'_>],
    fault: Option<Fault>,
) -> Result<Outcome, SkillError> {
    let work = update_dir(base);
    std::fs::create_dir_all(&work).map_err(|e| io(&work, e))?;
    std::fs::create_dir_all(skills_dir).map_err(|e| io(skills_dir, e))?;
    let mut entries = Vec::new();
    for change in changes {
        let name = &change.provenance.name;
        let staging = work.join(format!("stage-{name}"));
        remove_dir(&staging)?;
        if let Err(err) = copy_package(&change.package.dir, &staging) {
            let _ = remove_dir(&staging);
            for e in &entries {
                let JournalEntry { staging, .. } = e;
                let _ = remove_dir(staging);
            }
            return Err(err);
        }
        let target = skills_dir.join(name);
        entries.push(JournalEntry {
            name: name.clone(),
            original_exists: target.exists(),
            target,
            aside: work.join(format!("old-{name}")),
            staging,
        });
    }
    let prior_ledger = match std::fs::read_to_string(ledger_file) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(io(ledger_file, e)),
    };
    let mut journal = Journal {
        phase: "swapping".into(),
        ledger: ledger_file.to_path_buf(),
        prior_ledger,
        entries,
    };
    save_journal(base, &journal)?;

    let swapped = (|| {
        for (i, e) in journal.entries.iter().enumerate() {
            if fault == Some(Fault::StopAfterSwap(i)) {
                return Ok(Outcome::Stopped);
            }
            if e.original_exists {
                rename(&e.target, &e.aside)?;
            }
            if fault == Some(Fault::SwapFails(i)) {
                return Err(SkillError::Io {
                    path: e.target.display().to_string(),
                    source: std::io::Error::other("injected swap failure"),
                });
            }
            rename(&e.staging, &e.target)?;
        }
        if fault == Some(Fault::StopAfterSwap(journal.entries.len())) {
            return Ok(Outcome::Stopped);
        }
        let mut ledger = InstallLedger::load(ledger_file)?;
        for change in changes {
            ledger.record(change.provenance.clone());
        }
        if fault == Some(Fault::LedgerSave) {
            return Err(SkillError::Io {
                path: ledger_file.display().to_string(),
                source: std::io::Error::other("injected ledger save failure"),
            });
        }
        ledger.save()?;
        Ok(Outcome::Done)
    })();
    match swapped {
        Ok(Outcome::Stopped) => return Ok(Outcome::Stopped),
        Ok(Outcome::Done) => {}
        Err(err) => {
            roll_back(base, &journal)?;
            return Err(err);
        }
    }
    journal.phase = "committed".into();
    save_journal(base, &journal)?;
    if fault == Some(Fault::StopAfterCommit) {
        return Ok(Outcome::Stopped);
    }
    roll_forward(base, &journal)?;
    Ok(Outcome::Done)
}

fn roll_back(base: &Path, journal: &Journal) -> Result<(), SkillError> {
    for e in &journal.entries {
        if e.original_exists {
            // Swapped only once the old copy is aside; otherwise the target
            // is still the old copy and stays.
            if e.aside.exists() {
                remove_dir(&e.target)?;
                rename(&e.aside, &e.target)?;
            }
        } else if !e.staging.exists() {
            // Nothing was installed before: whatever the swap put there goes.
            remove_dir(&e.target)?;
        }
        remove_dir(&e.staging)?;
    }
    match &journal.prior_ledger {
        Some(text) => write_atomic(&journal.ledger, text.as_bytes())?,
        None => match std::fs::remove_file(&journal.ledger) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(&journal.ledger, e)),
        },
    }
    std::fs::remove_file(journal_path(base)).map_err(|e| io(&journal_path(base), e))
}

fn roll_forward(base: &Path, journal: &Journal) -> Result<(), SkillError> {
    for e in &journal.entries {
        remove_dir(&e.aside)?;
        remove_dir(&e.staging)?;
    }
    std::fs::remove_file(journal_path(base)).map_err(|e| io(&journal_path(base), e))
}

/// Finish or undo an interrupted update in this scope: roll a `swapping`
/// journal back, a `committed` one forward. Returns what it did.
///
/// # Errors
/// An unreadable journal, or a filesystem failure while recovering.
pub(crate) fn recover_update(base: &Path) -> Result<Option<String>, SkillError> {
    let path = journal_path(base);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io(&path, e)),
    };
    let journal: Journal = toml::from_str(&text).map_err(|e| {
        SkillError::Corrupt(format!("{} is not an update journal: {e}", path.display()))
    })?;
    let names: Vec<&str> = journal.entries.iter().map(|e| e.name.as_str()).collect();
    if journal.phase == "committed" {
        roll_forward(base, &journal)?;
        Ok(Some(format!(
            "finished an interrupted skills update ({})",
            names.join(", ")
        )))
    } else {
        roll_back(base, &journal)?;
        Ok(Some(format!(
            "rolled back an interrupted skills update ({})",
            names.join(", ")
        )))
    }
}

// --- cache swaps --------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct SwapMarker {
    id: String,
    final_dir: PathBuf,
    aside: PathBuf,
    /// The validated new snapshot. It stops existing only by being renamed
    /// into `final_dir`, so its presence says the swap has not happened.
    staging: PathBuf,
    new_commit: String,
    registry: PathBuf,
}

fn marker_path(repos: &Path, id: &str) -> PathBuf {
    repos.join(format!(".swap-{id}.toml"))
}

/// Move a validated `staging` snapshot into the cache slot of source `id` and,
/// when `registry` is given, record `new_commit` for it, as one recoverable
/// step. The old cache is kept until the registry is durable.
///
/// # Errors
/// A marker, rename or registry failure; a caught rename failure puts the old
/// cache back.
pub(crate) fn swap_cache(
    repos: &Path,
    id: &str,
    staging: &Path,
    new_commit: &str,
    registry: Option<&mut SourceRegistry>,
    fault: Option<Fault>,
) -> Result<Outcome, SkillError> {
    let final_dir = repos.join(id);
    let aside = repos.join(format!(".old-{id}"));
    remove_dir(&aside)?;
    let marker = SwapMarker {
        id: id.to_owned(),
        final_dir: final_dir.clone(),
        aside: aside.clone(),
        staging: staging.to_path_buf(),
        new_commit: new_commit.to_owned(),
        registry: registry
            .as_ref()
            .map(|r| r.path().to_path_buf())
            .unwrap_or_default(),
    };
    let text = toml::to_string_pretty(&marker)
        .map_err(|e| SkillError::Corrupt(format!("could not serialize a swap marker: {e}")))?;
    write_atomic(&marker_path(repos, id), text.as_bytes())?;
    if fault == Some(Fault::StopAfterMarker) {
        return Ok(Outcome::Stopped);
    }
    if final_dir.exists() {
        let aside_moved = if fault == Some(Fault::CacheAsideFails) {
            Err(SkillError::Io {
                path: aside.display().to_string(),
                source: std::io::Error::other("injected aside failure"),
            })
        } else {
            rename(&final_dir, &aside)
        };
        if let Err(err) = aside_moved {
            // Nothing moved: the old cache is still in place.
            std::fs::remove_file(marker_path(repos, id))
                .map_err(|e| io(&marker_path(repos, id), e))?;
            return Err(err);
        }
    }
    if fault == Some(Fault::StopAfterCacheAside) {
        return Ok(Outcome::Stopped);
    }
    let moved = if fault == Some(Fault::CacheSwap) {
        Err(SkillError::Io {
            path: final_dir.display().to_string(),
            source: std::io::Error::other("injected cache swap failure"),
        })
    } else {
        rename(staging, &final_dir)
    };
    if let Err(err) = moved {
        if aside.exists() {
            rename(&aside, &final_dir)?;
        }
        std::fs::remove_file(marker_path(repos, id)).map_err(|e| io(&marker_path(repos, id), e))?;
        return Err(err);
    }
    if fault == Some(Fault::StopAfterCacheFinal) {
        return Ok(Outcome::Stopped);
    }
    if let Some(registry) = registry {
        registry.set_commit(id, new_commit.to_owned())?;
        registry.save()?;
    }
    std::fs::remove_file(marker_path(repos, id)).map_err(|e| io(&marker_path(repos, id), e))?;
    remove_dir(&aside)?;
    Ok(Outcome::Done)
}

/// Finish or undo every interrupted cache swap under `repos`, then remove any
/// old cache left by a swap that finished but was stopped before its
/// cleanup. Returns what it did.
///
/// # Errors
/// An unreadable marker, or a filesystem or registry failure.
pub(crate) fn recover_caches(repos: &Path) -> Result<Vec<String>, SkillError> {
    let mut done = Vec::new();
    let Ok(listing) = std::fs::read_dir(repos) else {
        return Ok(done);
    };
    let mut markers = Vec::new();
    let mut olds = Vec::new();
    for entry in listing.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(".swap-") && name.ends_with(".toml") {
            markers.push(entry.path());
        } else if let Some(id) = name.strip_prefix(".old-") {
            olds.push((id.to_owned(), entry.path()));
        }
    }
    for path in markers {
        let text = std::fs::read_to_string(&path).map_err(|e| io(&path, e))?;
        let m: SwapMarker = toml::from_str(&text).map_err(|e| {
            SkillError::Corrupt(format!("{} is not a swap marker: {e}", path.display()))
        })?;
        if m.staging.exists() {
            // The new snapshot was never placed: undo whatever was set aside.
            if m.aside.exists() && !m.final_dir.exists() {
                rename(&m.aside, &m.final_dir)?;
            }
            remove_dir(&m.staging)?;
            std::fs::remove_file(&path).map_err(|e| io(&path, e))?;
            done.push(format!("rolled back an interrupted refresh of `{}`", m.id));
        } else if m.final_dir.exists() {
            if !m.registry.as_os_str().is_empty() {
                let mut registry = SourceRegistry::load(&m.registry)?;
                if registry
                    .find(&m.id)
                    .is_some_and(|s| s.commit != m.new_commit)
                {
                    registry.set_commit(&m.id, m.new_commit.clone())?;
                    registry.save()?;
                }
            }
            std::fs::remove_file(&path).map_err(|e| io(&path, e))?;
            remove_dir(&m.aside)?;
            done.push(format!("finished an interrupted refresh of `{}`", m.id));
        } else {
            if m.aside.exists() {
                rename(&m.aside, &m.final_dir)?;
            }
            std::fs::remove_file(&path).map_err(|e| io(&path, e))?;
            done.push(format!("rolled back an interrupted refresh of `{}`", m.id));
        }
    }
    // An old cache with no marker belongs to a swap that completed: its
    // registry commit is durable and the marker is gone, so it is garbage.
    for (id, path) in olds {
        if !marker_path(repos, &id).exists() {
            remove_dir(&path)?;
        }
    }
    Ok(done)
}

/// Whether an interrupted cache swap under `repos` still needs recovery.
pub(crate) fn cache_swap_pending(repos: &Path) -> bool {
    std::fs::read_dir(repos).is_ok_and(|listing| {
        listing.flatten().any(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with(".swap-") && n.ends_with(".toml")
        })
    })
}
