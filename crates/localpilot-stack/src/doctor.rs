//! What an installed stack looks like when it has gone wrong, and how to put it
//! right.
//!
//! Every install route publishes into one directory (`<localx root>/bin`), so a
//! stack binary found anywhere *else* on `PATH` is a leftover from an older
//! route — a `cargo install` copy in `~/.cargo/bin`, most often — and it wins
//! silently whenever its directory sits earlier on `PATH`. That failure is
//! invisible by construction: the install succeeds, reports success, and the
//! shell keeps running the old binary.
//!
//! This module names those copies (and the other residue an install can leave)
//! and removes them on request. It never removes anything on its own: the fix is
//! deleting executables, which is the user's call to make.

use std::io::Write;
use std::path::{Path, PathBuf};

use localpilot_dist::{executable_name, Cache};

use crate::{dev, shared_bin_dir, TRAIN};

/// A stack binary outside the managed directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Duplicate {
    pub tool: &'static str,
    /// The cargo package that would have installed it, for `cargo uninstall`.
    pub package: &'static str,
    pub path: PathBuf,
    /// Whether `PATH` resolves this copy rather than the managed one.
    pub wins: bool,
    /// Whether this copy is the executable running right now, which no process
    /// can delete from under itself on Windows.
    pub is_running: bool,
}

/// A file left behind by an install that has no reason to exist any more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stray {
    pub path: PathBuf,
    pub reason: &'static str,
}

/// A cached release build that nothing can select any more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleVersion {
    pub tool: &'static str,
    pub label: String,
    pub dir: PathBuf,
    pub bytes: u64,
}

/// Everything wrong with the current install, as one value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub bin: PathBuf,
    /// Whether the managed directory is on `PATH` at all.
    pub on_path: bool,
    pub duplicates: Vec<Duplicate>,
    pub strays: Vec<Stray>,
    pub stale: Vec<StaleVersion>,
    /// The pinned development workspace, when one is set.
    pub workspace: Option<PathBuf>,
}

impl Report {
    /// Whether anything needs fixing.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.on_path
            && self.duplicates.is_empty()
            && self.strays.is_empty()
            && self.stale.is_empty()
    }

    /// Bytes the stale caches would free.
    #[must_use]
    pub fn stale_bytes(&self) -> u64 {
        self.stale.iter().map(|version| version.bytes).sum()
    }
}

/// Inspect the installed stack. `None` when the platform reports no per-user
/// data directory, where there is no managed install to reason about.
#[must_use]
pub fn diagnose() -> Option<Report> {
    let bin = shared_bin_dir()?;
    let entries: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    let workspace = dev::pinned();
    let running = std::env::current_exe().ok();

    let duplicates = duplicates_on_path(&bin, &entries, running.as_deref());
    // Residue is looked for in the managed directory and in every `PATH` entry,
    // not only where a duplicate still sits. A duplicate that was in use gets
    // renamed rather than deleted, so the directory holding the leftover is
    // precisely the one that no longer has a duplicate to point at it.
    let mut dirs: Vec<PathBuf> = entries.clone();
    dirs.push(bin.clone());
    dirs.sort();
    dirs.dedup();

    Some(Report {
        on_path: entries.iter().any(|entry| *entry == bin),
        bin,
        duplicates,
        strays: {
            let mut strays = strays_in(&dirs);
            strays.extend(cargo_registry_residue());
            strays
        },
        // Cached release archives are what the release channel activates from.
        // In development mode nothing activates them — every binary is built —
        // so the whole cache is residue rather than a rollback path.
        stale: if workspace.is_some() {
            cached_versions()
        } else {
            Vec::new()
        },
        workspace,
    })
}

/// Stack binaries `PATH` offers from anywhere but the managed directory.
fn duplicates_on_path(bin: &Path, entries: &[PathBuf], running: Option<&Path>) -> Vec<Duplicate> {
    let cutoff = entries
        .iter()
        .position(|entry| entry == bin)
        .unwrap_or(entries.len());
    let mut found = Vec::new();
    for tool in TRAIN {
        let name = executable_name(tool.tool);
        for (index, entry) in entries.iter().enumerate() {
            if entry == bin {
                continue;
            }
            let candidate = entry.join(&name);
            if !candidate.is_file() {
                continue;
            }
            found.push(Duplicate {
                tool: tool.tool,
                package: tool.package,
                wins: index < cutoff,
                is_running: running.is_some_and(|running| same_file(running, &candidate)),
                path: candidate,
            });
        }
    }
    found
}

/// Install residue in `dirs`: the displaced and half-written copies an
/// interrupted swap leaves, and the dated backups older installers wrote.
fn strays_in(dirs: &[PathBuf]) -> Vec<Stray> {
    let mut strays = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            let Some(reason) = stray_reason(&name) else {
                continue;
            };
            strays.push(Stray { path, reason });
        }
    }
    strays
}

/// Why a file name is residue, or `None` when it is not.
///
/// Matching is on the *stack's own* suffixes only. A directory holding a
/// duplicate is usually `~/.cargo/bin`, full of binaries this command has no
/// business judging, so anything that is not recognisably one of ours is left
/// alone.
fn stray_reason(name: &str) -> Option<&'static str> {
    let is_ours = TRAIN
        .iter()
        .any(|tool| name.starts_with(&executable_name(tool.tool).to_ascii_lowercase()));
    if !is_ours {
        return None;
    }
    if name.ends_with(".displaced") {
        return Some("displaced by an install whose sweep could not remove it");
    }
    if name.ends_with(".incoming") {
        return Some("a half-written install that never completed");
    }
    if name.ends_with(".old") {
        return Some("displaced by an older installer");
    }
    if name.contains(".pre-") {
        return Some("a dated backup from an older installer");
    }
    None
}

/// cargo's own registry files, when an old `cargo install --root <localx root>`
/// left them in the managed root.
///
/// They describe binaries this stack now installs itself, so they are stale the
/// moment the stack takes over — and they are what makes cargo refuse a later
/// uninstall as corrupt metadata once the binary they name is gone.
fn cargo_registry_residue() -> Vec<Stray> {
    let Some(root) = Cache::default_root("localx") else {
        return Vec::new();
    };
    [".crates.toml", ".crates2.json"]
        .into_iter()
        .map(|name| root.join(name))
        .filter(|path| path.is_file())
        .map(|path| Stray {
            path,
            reason: "cargo's registry from an install into this directory;                      the stack manages these binaries itself",
        })
        .collect()
}

/// Every cached release build of every train tool.
fn cached_versions() -> Vec<StaleVersion> {
    TRAIN
        .iter()
        .filter_map(|tool| Cache::default_root(tool.tool).map(|root| (tool, Cache::new(root))))
        .flat_map(|(tool, cache)| {
            cache
                .installed()
                .into_iter()
                .map(|installed| StaleVersion {
                    tool: tool.tool,
                    label: installed.version.to_dir_name(),
                    bytes: directory_bytes(&installed.dir),
                    dir: installed.dir,
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Recursive size of a directory; zero when it cannot be read, which keeps a
/// permissions problem from failing a report that is only advisory.
fn directory_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => directory_bytes(&entry.path()),
            Ok(_) => entry.metadata().map(|meta| meta.len()).unwrap_or(0),
            Err(_) => 0,
        })
        .sum()
}

/// Whether two paths name the same file, tolerant of the `\\?\` prefix and the
/// case differences `canonicalize` normalises.
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Print the diagnosis, saying for each finding what it costs and what removes
/// it.
///
/// # Errors
/// Returns an error only if output cannot be written.
pub fn write_report(report: &Report, out: &mut dyn Write) -> anyhow::Result<()> {
    writeln!(out, "managed binaries: {}", report.bin.display())?;
    match &report.workspace {
        Some(workspace) => writeln!(
            out,
            "channel:          development — built from {}",
            workspace.display()
        )?,
        None => writeln!(out, "channel:          release")?,
    }

    if !report.on_path {
        writeln!(
            out,
            "\nPATH does not include the managed directory, so nothing installed there runs."
        )?;
        writeln!(out, "    {}", report.bin.display())?;
    }

    if !report.duplicates.is_empty() {
        writeln!(
            out,
            "\nstack binaries outside the managed directory ({}):",
            report.duplicates.len()
        )?;
        for duplicate in &report.duplicates {
            let mark = if duplicate.wins {
                " <- PATH resolves this one"
            } else {
                ""
            };
            writeln!(
                out,
                "    {:<11} {}{mark}",
                duplicate.tool,
                duplicate.path.display()
            )?;
        }
        writeln!(
            out,
            "every install route publishes into the managed directory, so these are leftovers."
        )?;
    }

    if !report.strays.is_empty() {
        writeln!(out, "\ninstall residue ({}):", report.strays.len())?;
        for stray in &report.strays {
            writeln!(out, "    {} — {}", stray.path.display(), stray.reason)?;
        }
    }

    if !report.stale.is_empty() {
        writeln!(
            out,
            "\ncached release builds nothing can select in development mode ({}, {:.0} MB):",
            report.stale.len(),
            bytes_as_mb(report.stale_bytes())
        )?;
        for stale in &report.stale {
            writeln!(out, "    {:<11} {}", stale.tool, stale.dir.display())?;
        }
    }

    if report.is_clean() {
        writeln!(out, "\nnothing to fix.")?;
    } else {
        writeln!(out, "\nrun `localx doctor --fix` to remove the above.")?;
    }
    Ok(())
}

/// Remove everything the report found: the duplicate binaries (and their cargo
/// registrations), the residue, and the unusable caches.
///
/// The running executable is never removed — Windows would refuse and every
/// other platform would leave the caller without a binary — so it is reported
/// instead, with the one instruction that fixes it.
///
/// # Errors
/// Returns an error only if output cannot be written; a file that cannot be
/// removed is reported and the rest of the fix continues.
pub fn fix(report: &Report, out: &mut dyn Write) -> anyhow::Result<()> {
    for duplicate in &report.duplicates {
        if duplicate.is_running {
            writeln!(
                out,
                "kept {} — it is the executable running right now.",
                duplicate.path.display()
            )?;
            writeln!(
                out,
                "  run {} and repeat the fix, or delete this file afterwards.",
                report.bin.join(executable_name(duplicate.tool)).display()
            )?;
            continue;
        }
        // Uninstalling through cargo first keeps its own registry
        // (`~/.cargo/.crates.toml`) honest. It is best effort: the copy may
        // predate that registry, or name a package spec that now matches more
        // than one entry, and the file still has to go either way.
        let uninstalled = cargo_uninstall(duplicate);
        if !duplicate.path.exists() {
            writeln!(
                out,
                "removed {} (via cargo uninstall)",
                duplicate.path.display()
            )?;
            continue;
        }
        match std::fs::remove_file(&duplicate.path) {
            Ok(()) => writeln!(
                out,
                "removed {}{}",
                duplicate.path.display(),
                if uninstalled {
                    " (and its cargo registration)"
                } else {
                    ""
                }
            )?,
            // A duplicate that is *running* — an editor's MCP server, a serve
            // command — cannot be deleted while it holds the image, and waiting
            // for it is not something this command can do. Renaming it is
            // permitted where deleting is not, and it is the part that matters:
            // the name `PATH` resolves is gone immediately, the bytes go on the
            // next run's residue sweep.
            Err(error) => match displace(&duplicate.path) {
                Some(aside) => {
                    writeln!(
                        out,
                        "{} is in use, so it was renamed to {} instead of deleted.",
                        duplicate.path.display(),
                        aside.display()
                    )?;
                    writeln!(
                        out,
                        "  PATH no longer resolves it; the next `localx doctor --fix` sweeps \
                         the file once nothing holds it."
                    )?;
                }
                None => writeln!(
                    out,
                    "could not remove {}: {error}",
                    duplicate.path.display()
                )?,
            },
        }
    }

    for stray in &report.strays {
        match std::fs::remove_file(&stray.path) {
            Ok(()) => writeln!(out, "removed {}", stray.path.display())?,
            Err(error) => writeln!(out, "could not remove {}: {error}", stray.path.display())?,
        }
    }

    let mut freed = 0;
    for stale in &report.stale {
        match std::fs::remove_dir_all(&stale.dir) {
            Ok(()) => {
                freed += stale.bytes;
                writeln!(out, "removed {}", stale.dir.display())?;
            }
            Err(error) => writeln!(out, "could not remove {}: {error}", stale.dir.display())?,
        }
    }
    if freed > 0 {
        writeln!(
            out,
            "freed {:.0} MB of cached release builds.",
            bytes_as_mb(freed)
        )?;
    }

    if !report.on_path {
        writeln!(
            out,
            "\nPATH still has to include the managed directory; nothing here can set it for you:"
        )?;
        writeln!(out, "    {}", report.bin.display())?;
    }
    Ok(())
}

/// Rename a file out of the way, returning where it went.
///
/// The one operation an operating system still allows on a running executable.
/// `None` when even that is refused, which is a real failure and is reported as
/// one.
fn displace(path: &Path) -> Option<PathBuf> {
    let aside = path.with_extension(format!(
        "{}displaced",
        path.extension()
            .map(|extension| format!("{}.", extension.to_string_lossy()))
            .unwrap_or_default()
    ));
    let _ = std::fs::remove_file(&aside);
    std::fs::rename(path, &aside).ok().map(|()| aside)
}

/// Ask cargo to forget a copy it installed. Whether it *was* a cargo install is
/// not knowable from the path alone, so failure is ordinary and silent.
fn cargo_uninstall(duplicate: &Duplicate) -> bool {
    if !duplicate
        .path
        .components()
        .any(|component| component.as_os_str() == ".cargo")
    {
        return false;
    }
    let Ok(output) = std::process::Command::new("cargo")
        .args(["uninstall", duplicate.package])
        .output()
    else {
        return false;
    };
    if output.status.success() {
        return true;
    }
    // A bare package name can match more than one installed entry — the same
    // tool installed once from git and once from a path, say — and cargo then
    // refuses and lists the exact specs. Retrying with those is the difference
    // between clearing the registry and leaving an entry whose binary this
    // command is about to delete, which cargo later calls corrupt metadata and
    // refuses to touch at all.
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    ambiguous_specs(&stderr, duplicate.package)
        .into_iter()
        .any(|spec| {
            std::process::Command::new("cargo")
                .args(["uninstall", &spec])
                .output()
                .is_ok_and(|retry| retry.status.success())
        })
}

/// The `package@version` specs cargo lists when a bare name is ambiguous.
///
/// Parsed rather than guessed: the versions are cargo's own answer, and the
/// alternative — reading `.crates.toml` — is reaching into cargo's private
/// state to do what its own CLI will do when asked precisely.
fn ambiguous_specs(stderr: &str, package: &str) -> Vec<String> {
    let prefix = format!("{package}@");
    stderr
        .split_whitespace()
        .filter(|token| token.starts_with(&prefix) && token.len() > prefix.len())
        .map(|token| token.trim_end_matches(',').to_string())
        .collect()
}

fn bytes_as_mb(bytes: u64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let mb = bytes as f64 / (1024.0 * 1024.0);
    mb
}

#[cfg(test)]
mod tests {
    use super::{duplicates_on_path, stray_reason, strays_in};
    use std::path::PathBuf;

    #[test]
    fn a_copy_in_an_earlier_entry_is_the_one_path_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let cargo = dir.path().join("cargo");
        let managed = dir.path().join("managed");
        std::fs::create_dir_all(&cargo).unwrap();
        std::fs::create_dir_all(&managed).unwrap();
        let name = localpilot_dist::executable_name("localmind");
        std::fs::write(cargo.join(&name), "old").unwrap();

        let found = duplicates_on_path(&managed, &[cargo.clone(), managed.clone()], None);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].tool, "localmind");
        assert_eq!(found[0].package, "localmind-cli");
        assert!(found[0].wins);
        assert!(!found[0].is_running);
    }

    #[test]
    fn a_copy_in_a_later_entry_is_still_reported_but_does_not_win() {
        let dir = tempfile::tempdir().unwrap();
        let managed = dir.path().join("managed");
        let other = dir.path().join("other");
        std::fs::create_dir_all(&managed).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(
            other.join(localpilot_dist::executable_name("localbox")),
            "x",
        )
        .unwrap();

        let found = duplicates_on_path(&managed, &[managed.clone(), other], None);
        assert_eq!(found.len(), 1);
        assert!(!found[0].wins);
    }

    #[test]
    fn the_managed_directory_is_never_its_own_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let managed = dir.path().join("managed");
        std::fs::create_dir_all(&managed).unwrap();
        std::fs::write(
            managed.join(localpilot_dist::executable_name("localx")),
            "x",
        )
        .unwrap();

        assert!(duplicates_on_path(&managed, &[managed.clone()], None).is_empty());
    }

    #[test]
    fn residue_is_recognised_only_for_this_stacks_own_files() {
        assert!(stray_reason("localx.exe.displaced").is_some());
        assert!(stray_reason("localpilot.exe.incoming").is_some());
        assert!(stray_reason("localx.exe.pre-shortcuts-20260821").is_some());
        assert!(stray_reason("localx.exe.old").is_some());
        assert_eq!(stray_reason("localx.exe"), None);
        assert_eq!(stray_reason("ripgrep.exe.displaced"), None);
        assert_eq!(stray_reason("cargo-nextest.exe"), None);
    }

    #[test]
    fn an_ambiguous_uninstall_is_retried_with_the_specs_cargo_named() {
        // cargo's own wording when a tool was installed twice (from git and
        // from a path, here). Without the retry the registry keeps an entry
        // whose binary is then deleted, and cargo refuses every later
        // uninstall with "corrupt metadata".
        let stderr = "error: There are multiple `localbox` packages in your project, \
                      and the specification `localbox` is ambiguous.\n\
                      Please re-run this command with one of the following \
                      specifications:\n  localbox@3.3.2\n  localbox@5.0.0\n";
        assert_eq!(
            super::ambiguous_specs(stderr, "localbox"),
            vec!["localbox@3.3.2".to_string(), "localbox@5.0.0".to_string()]
        );
        assert!(super::ambiguous_specs(
            "error: package ID specification `x` did not match any packages",
            "localbox"
        )
        .is_empty());
    }

    #[test]
    fn a_file_that_cannot_be_deleted_is_renamed_to_recognisable_residue() {
        // The live case: a duplicate held open by a running editor process. The
        // rename is what removes it from PATH; the sweep takes the bytes later,
        // which only works if the new name is one `stray_reason` recognises.
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join(localpilot_dist::executable_name("localpilot"));
        std::fs::write(&path, "x").unwrap();

        let aside = super::displace(&path).expect("a rename is permitted");
        assert!(!path.exists());
        assert!(aside.is_file());
        let name = aside
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_ascii_lowercase();
        assert!(stray_reason(&name).is_some(), "{name}");
    }

    #[test]
    fn residue_is_collected_from_every_named_directory() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let keep = bin.join(localpilot_dist::executable_name("localx"));
        std::fs::write(&keep, "x").unwrap();
        let drop = bin.join(format!(
            "{}.displaced",
            localpilot_dist::executable_name("localx")
        ));
        std::fs::write(&drop, "x").unwrap();

        let strays = strays_in(&[bin]);
        assert_eq!(strays.len(), 1);
        assert_eq!(strays[0].path, drop);
        assert!(PathBuf::from(&keep).is_file());
    }
}
