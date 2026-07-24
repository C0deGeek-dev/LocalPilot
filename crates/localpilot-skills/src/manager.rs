//! The skill-source management service: the one contract shared by the
//! `localpilot skills ...` CLI and the `/skills ...` slash surface (LocalHub#40).
//!
//! Everything user-facing about sources and managed installs flows through
//! [`SkillsManager`]: registering and refreshing public HTTPS Git snapshots,
//! searching cached catalogs offline, installing and removing managed skill
//! packages, and listing sources. The manager owns the safety invariants —
//! trust-gated project mutations, an extra disclosure for global scope, staged
//! atomic fetches, all-or-nothing bulk installs, and never overwriting or deleting
//! content it did not install — so both surfaces inherit them identically.
//!
//! Side effects are seams: the network is a [`RepoFetcher`], the clock is an
//! injected `now` string, and confirmation is an [`Approval`]. Nothing in a
//! fetched package is executed and no permission is granted; management only moves
//! validated files and records provenance.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::catalog::{read_catalog, Catalog};
use crate::error::SkillError;
use crate::fetch::{ensure_snapshot_within_bounds, RepoFetcher, Snapshot};
use crate::install::{delete_installed, install_package, InstallLedger, Provenance};
use crate::source::{normalize_url, source_id, SkillSource, SourceRegistry};

/// The scope a mutation targets: the current project or the user-global baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// `<project>/.localpilot/` — the default when `-g` is absent.
    Project,
    /// `~/.localpilot/` — the user-global scope (`-g`).
    Global,
}

impl Scope {
    fn label(self) -> &'static str {
        match self {
            Scope::Project => "project",
            Scope::Global => "global",
        }
    }
}

/// The scope a read-only command reports over: the effective global+project view
/// (no `-g`) or the global scope alone (`-g`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadScope {
    /// The effective global baseline plus the project overlay.
    Effective,
    /// The global scope only.
    GlobalOnly,
}

/// What an install targets.
#[derive(Debug, Clone)]
pub enum InstallSpec {
    /// One named skill, optionally pinned to a source id (`--repo`).
    Named { name: String, repo: Option<String> },
    /// Every package of one source (`--all --repo <id>`).
    All { repo: String },
}

/// A yes/no confirmation seam so the manager never reads stdin itself.
pub trait Confirm {
    /// Ask `question` and return whether the user approved.
    fn confirm(&mut self, question: &str) -> bool;
}

/// How a mutation is approved. The manager always discloses the impact first;
/// this decides what happens next.
pub enum Approval<'a> {
    /// Approval already given (`--yes`): proceed after disclosure.
    AssumeYes,
    /// Interactive terminal: disclose, then ask via the [`Confirm`] seam.
    Interactive(&'a mut dyn Confirm),
    /// No terminal and no `--yes`: disclose, then refuse rather than act unattended.
    NonInteractive,
}

/// Per-scope on-disk locations under a scope base directory.
struct ScopePaths {
    base: PathBuf,
}

impl ScopePaths {
    fn sources_file(&self) -> PathBuf {
        self.base.join("skill-sources.toml")
    }
    fn ledger_file(&self) -> PathBuf {
        self.base.join("installed-skills.toml")
    }
    fn repos_dir(&self) -> PathBuf {
        self.base.join("skill-repos")
    }
    fn skills_dir(&self) -> PathBuf {
        self.base.join("skills")
    }
    fn cache_for(&self, id: &str) -> PathBuf {
        self.repos_dir().join(id)
    }
}

/// The management service. Constructed per invocation with the current project,
/// the per-user home (for the global scope), workspace trust, a network fetcher,
/// and an injected timestamp.
pub struct SkillsManager<'a> {
    project_root: &'a Path,
    home: Option<&'a Path>,
    trusted: bool,
    fetcher: &'a dyn RepoFetcher,
    now: &'a str,
}

impl<'a> SkillsManager<'a> {
    /// Construct a manager. `home` is `None` when no home directory resolves, in
    /// which case global operations fail clearly and project behavior is intact.
    #[must_use]
    pub fn new(
        project_root: &'a Path,
        home: Option<&'a Path>,
        trusted: bool,
        fetcher: &'a dyn RepoFetcher,
        now: &'a str,
    ) -> Self {
        Self {
            project_root,
            home,
            trusted,
            fetcher,
            now,
        }
    }

    // --- scope resolution -------------------------------------------------

    fn paths(&self, scope: Scope) -> Result<ScopePaths, SkillError> {
        let base = match scope {
            Scope::Project => self.project_root.join(".localpilot"),
            Scope::Global => self
                .home
                .ok_or_else(|| {
                    SkillError::Refused(
                        "no home directory resolves; global skill management is unavailable"
                            .to_string(),
                    )
                })?
                .join(".localpilot"),
        };
        Ok(ScopePaths { base })
    }

    /// The scopes a read-only command spans, most-specific last so a project entry
    /// is shown after the global baseline it overlays.
    fn read_scopes(&self, read: ReadScope) -> Vec<(Scope, ScopePaths)> {
        let mut scopes = Vec::new();
        if self.home.is_some() {
            if let Ok(paths) = self.paths(Scope::Global) {
                scopes.push((Scope::Global, paths));
            }
        }
        if matches!(read, ReadScope::Effective) {
            if let Ok(paths) = self.paths(Scope::Project) {
                scopes.push((Scope::Project, paths));
            }
        }
        scopes
    }

    /// A project mutation requires a trusted workspace; a global mutation only
    /// requires a resolvable home (checked in [`Self::paths`]).
    fn ensure_mutable(&self, scope: Scope) -> Result<(), SkillError> {
        if scope == Scope::Project && !self.trusted {
            return Err(SkillError::Refused(
                "this workspace is not trusted; project skill state cannot be modified".to_string(),
            ));
        }
        Ok(())
    }

    /// Disclose `impact`, adding a global-scope warning, then apply the approval
    /// policy. Returns `Ok(())` to proceed; an unapproved mutation is an error so
    /// the CLI exits non-zero rather than silently doing nothing.
    fn gate(
        &self,
        out: &mut dyn Write,
        approval: Approval<'_>,
        scope: Scope,
        impact: &str,
    ) -> Result<(), SkillError> {
        if scope == Scope::Global {
            line(
                out,
                "GLOBAL scope: this affects skills for every project under your user account.",
            )?;
        }
        line(out, impact)?;
        match approval {
            Approval::AssumeYes => Ok(()),
            Approval::Interactive(confirm) => {
                if confirm.confirm("Proceed?") {
                    Ok(())
                } else {
                    Err(SkillError::Refused("cancelled".to_string()))
                }
            }
            Approval::NonInteractive => Err(SkillError::Refused(
                "approval required; re-run with --yes to proceed unattended".to_string(),
            )),
        }
    }

    // --- repository management --------------------------------------------

    /// Register a public HTTPS source: validate the URL, fetch one snapshot,
    /// verify its catalog, cache it, and record the commit. Installs nothing.
    ///
    /// # Errors
    /// Rejects a bad URL, refuses an untrusted/unapproved mutation, and reports a
    /// fetch or catalog-validation failure without leaving a partial cache.
    pub fn repo_add(
        &self,
        scope: Scope,
        url: &str,
        approval: Approval<'_>,
        out: &mut dyn Write,
    ) -> Result<(), SkillError> {
        self.ensure_mutable(scope)?;
        let normalized = normalize_url(url)?;
        let id = source_id(&normalized);
        let paths = self.paths(scope)?;
        let mut registry = SourceRegistry::load(&paths.sources_file())?;
        if registry.find(&normalized).is_some() {
            return Err(SkillError::Conflict(format!(
                "`{normalized}` is already a registered source; use `skills repo refresh` to update it"
            )));
        }

        self.gate(
            out,
            approval,
            scope,
            &format!(
                "Fetch (network) {normalized} into the {} cache at {}.",
                scope.label(),
                paths.cache_for(&id).display()
            ),
        )?;

        let snapshot = self.fetch_snapshot_into_cache(&paths, &id, &normalized)?;
        let catalog = read_catalog(&paths.cache_for(&id))?;
        registry.add(SkillSource {
            id: id.clone(),
            url: normalized.clone(),
            commit: snapshot.commit.clone(),
            added_at: self.now.to_string(),
        })?;
        registry.save()?;
        line(
            out,
            &format!(
                "added source `{id}` ({normalized}) @ {} — {} skill(s) available; installed nothing.",
                short(&snapshot.commit),
                catalog.packages.len()
            ),
        )
    }

    /// Refresh one source (or all): fetch a fresh snapshot and atomically replace
    /// the cache. A network or validation failure leaves the previous cache in
    /// place, and installed skills are never changed.
    ///
    /// # Errors
    /// Refuses an untrusted/unapproved mutation; a per-source failure is reported
    /// but does not corrupt the cache.
    pub fn repo_refresh(
        &self,
        scope: Scope,
        url: Option<&str>,
        approval: Approval<'_>,
        out: &mut dyn Write,
    ) -> Result<(), SkillError> {
        self.ensure_mutable(scope)?;
        let paths = self.paths(scope)?;
        let mut registry = SourceRegistry::load(&paths.sources_file())?;
        let targets: Vec<SkillSource> = match url {
            Some(u) => vec![registry
                .find(u)
                .cloned()
                .ok_or_else(|| SkillError::NotFound(format!("no registered source `{u}`")))?],
            None => registry.sources().to_vec(),
        };
        if targets.is_empty() {
            return Err(SkillError::NotFound(format!(
                "no sources registered in the {} scope",
                scope.label()
            )));
        }

        self.gate(
            out,
            approval,
            scope,
            &format!(
                "Refresh (network) {} source(s) in the {} scope; installed skills are unchanged.",
                targets.len(),
                scope.label()
            ),
        )?;

        let mut failures = 0usize;
        for source in &targets {
            match self.refresh_one(&paths, source) {
                Ok(snapshot) => {
                    registry.set_commit(&source.id, snapshot.commit.clone())?;
                    line(
                        out,
                        &format!("refreshed `{}` @ {}", source.id, short(&snapshot.commit)),
                    )?;
                }
                Err(err) => {
                    failures += 1;
                    line(
                        out,
                        &format!(
                            "could not refresh `{}`: {err} (previous cache kept)",
                            source.id
                        ),
                    )?;
                }
            }
        }
        registry.save()?;
        if failures > 0 {
            return Err(SkillError::Fetch(format!(
                "{failures} of {} source(s) failed to refresh",
                targets.len()
            )));
        }
        Ok(())
    }

    /// List registered sources for the read scope (effective, or global-only).
    ///
    /// # Errors
    /// Returns an error only if reading a registry or writing output fails.
    pub fn repo_list(&self, read: ReadScope, out: &mut dyn Write) -> Result<(), SkillError> {
        let mut any = false;
        for (scope, paths) in self.read_scopes(read) {
            let registry = SourceRegistry::load(&paths.sources_file())?;
            for source in registry.sources() {
                any = true;
                line(
                    out,
                    &format!(
                        "- {} [{}] {} @ {}",
                        source.id,
                        scope.label(),
                        source.url,
                        short(&source.commit)
                    ),
                )?;
            }
        }
        if !any {
            line(out, "no skill sources registered")?;
        }
        Ok(())
    }

    /// Remove a source's registration and cache. Installed skills remain usable
    /// with their recorded provenance.
    ///
    /// # Errors
    /// Refuses an untrusted/unapproved mutation; errors if the source is unknown.
    pub fn repo_delete(
        &self,
        scope: Scope,
        url: &str,
        approval: Approval<'_>,
        out: &mut dyn Write,
    ) -> Result<(), SkillError> {
        self.ensure_mutable(scope)?;
        let paths = self.paths(scope)?;
        let mut registry = SourceRegistry::load(&paths.sources_file())?;
        let source = registry
            .find(url)
            .cloned()
            .ok_or_else(|| SkillError::NotFound(format!("no registered source `{url}`")))?;

        self.gate(
            out,
            approval,
            scope,
            &format!(
                "Remove source `{}` ({}) and its cache from the {} scope; installed skills stay.",
                source.id,
                source.url,
                scope.label()
            ),
        )?;

        registry.remove(&source.id)?;
        registry.save()?;
        let cache = paths.cache_for(&source.id);
        if cache.exists() {
            std::fs::remove_dir_all(&cache).map_err(|src| SkillError::Io {
                path: cache.display().to_string(),
                source: src,
            })?;
        }
        line(out, &format!("removed source `{}`", source.id))
    }

    // --- discovery --------------------------------------------------------

    /// Search cached catalogs (no network) for packages matching `query`, showing
    /// source, commit, description, scope, install state, and marking a name that
    /// is ambiguous across sources.
    ///
    /// # Errors
    /// Returns an error only if writing output fails.
    pub fn available(
        &self,
        read: ReadScope,
        query: Option<&str>,
        out: &mut dyn Write,
    ) -> Result<(), SkillError> {
        let catalogs = self.load_catalogs(read, out)?;
        let query = query.map(str::to_ascii_lowercase);
        // Count each name across sources so an ambiguous one can be flagged.
        let mut name_counts: std::collections::BTreeMap<&str, usize> =
            std::collections::BTreeMap::new();
        for entry in &catalogs {
            for package in &entry.catalog.packages {
                *name_counts.entry(package.name.as_str()).or_default() += 1;
            }
        }

        let mut shown = 0usize;
        for entry in &catalogs {
            let ledger = InstallLedger::load(&entry.paths.ledger_file())?;
            for package in &entry.catalog.packages {
                if let Some(q) = &query {
                    let hay = format!(
                        "{} {}",
                        package.name.to_ascii_lowercase(),
                        package.description.to_ascii_lowercase()
                    );
                    if !hay.contains(q) {
                        continue;
                    }
                }
                shown += 1;
                let installed = if ledger.get(&package.name).is_some() {
                    "installed"
                } else {
                    "available"
                };
                let ambiguous = if name_counts.get(package.name.as_str()).copied().unwrap_or(0) > 1
                {
                    " [ambiguous: several sources — use --repo <id>]"
                } else {
                    ""
                };
                line(
                    out,
                    &format!(
                        "- {} [{}] ({}, {} @ {}){}: {}",
                        package.name,
                        installed,
                        entry.scope.label(),
                        entry.source.id,
                        short(&entry.source.commit),
                        ambiguous,
                        package.description
                    ),
                )?;
            }
        }
        if shown == 0 {
            line(out, "no matching skills in any cached source")?;
        }
        Ok(())
    }

    // --- installation -----------------------------------------------------

    /// Install a named skill or every package of a source into `scope`, from the
    /// cached catalogs. Never overwrites a same-scope skill; a bulk install is
    /// all-or-nothing.
    ///
    /// # Errors
    /// Refuses an untrusted/unapproved mutation; errors on an unknown or ambiguous
    /// name, a same-scope conflict, or an over-bound package.
    pub fn install(
        &self,
        scope: Scope,
        spec: InstallSpec,
        approval: Approval<'_>,
        out: &mut dyn Write,
    ) -> Result<(), SkillError> {
        self.ensure_mutable(scope)?;
        // Install draws from the sources visible to the scope: a project install
        // may draw from the effective (project+global) sources; a global install
        // only from global sources.
        let read = match scope {
            Scope::Project => ReadScope::Effective,
            Scope::Global => ReadScope::GlobalOnly,
        };
        let catalogs = self.load_catalogs(read, out)?;

        // Resolve the concrete (source, package) pairs to install.
        let chosen = match &spec {
            InstallSpec::All { repo } => {
                let entry = catalogs
                    .iter()
                    .find(|c| c.source.id == *repo || c.source.url == *repo)
                    .ok_or_else(|| {
                        SkillError::NotFound(format!("no cached source `{repo}` in this scope"))
                    })?;
                entry
                    .catalog
                    .packages
                    .iter()
                    .map(|p| (entry, p))
                    .collect::<Vec<_>>()
            }
            InstallSpec::Named { name, repo } => {
                let mut hits: Vec<(&SourceCatalog, &crate::catalog::CatalogPackage)> = catalogs
                    .iter()
                    .filter(|c| {
                        repo.as_ref()
                            .is_none_or(|r| c.source.id == *r || c.source.url == *r)
                    })
                    .filter_map(|c| c.catalog.package(name).map(|p| (c, p)))
                    .collect();
                if hits.is_empty() {
                    return Err(SkillError::NotFound(format!(
                        "no cached skill named `{name}`{}",
                        repo.as_ref()
                            .map(|r| format!(" in source `{r}`"))
                            .unwrap_or_default()
                    )));
                }
                if hits.len() > 1 {
                    return Err(SkillError::Rejected(format!(
                        "`{name}` is offered by several sources; pick one with --repo <id>"
                    )));
                }
                vec![hits.remove(0)]
            }
        };

        let paths = self.paths(scope)?;
        let skills_dir = paths.skills_dir();
        // Preflight: a bulk install is all-or-nothing, so refuse before writing
        // anything if any target already exists in this scope.
        for (_, package) in &chosen {
            if skills_dir.join(&package.name).exists() {
                return Err(SkillError::Conflict(format!(
                    "a skill named `{}` already exists in the {} scope; nothing was installed",
                    package.name,
                    scope.label()
                )));
            }
        }

        let names: Vec<&str> = chosen.iter().map(|(_, p)| p.name.as_str()).collect();
        self.gate(
            out,
            approval,
            scope,
            &format!(
                "Install {} skill(s) [{}] into {} ({}). Runs nothing; grants nothing.",
                chosen.len(),
                names.join(", "),
                scope.label(),
                skills_dir.display()
            ),
        )?;

        let mut ledger = InstallLedger::load(&paths.ledger_file())?;
        let mut installed: Vec<String> = Vec::new();
        for (entry, package) in &chosen {
            let provenance = Provenance {
                name: package.name.clone(),
                source_id: entry.source.id.clone(),
                source_url: entry.source.url.clone(),
                commit: entry.source.commit.clone(),
                source_path: package.source_path.clone(),
                scope: scope.label().to_string(),
                installed_at: self.now.to_string(),
            };
            match install_package(&skills_dir, &mut ledger, package, provenance) {
                Ok(_) => installed.push(package.name.clone()),
                Err(err) => {
                    // Roll back a partial bulk install so it stays all-or-nothing.
                    for name in &installed {
                        let _ = delete_installed(&skills_dir, &mut ledger, name);
                    }
                    return Err(err);
                }
            }
        }
        line(
            out,
            &format!(
                "installed {} skill(s): {}",
                installed.len(),
                installed.join(", ")
            ),
        )
    }

    /// Remove a managed installed skill from `scope`. Only a LocalPilot-installed
    /// skill is removed; hand-authored content is refused. Removing a project
    /// install reveals any global skill of the same name again.
    ///
    /// # Errors
    /// Refuses an untrusted/unapproved mutation or a delete of unmanaged content.
    pub fn delete(
        &self,
        scope: Scope,
        name: &str,
        approval: Approval<'_>,
        out: &mut dyn Write,
    ) -> Result<(), SkillError> {
        self.ensure_mutable(scope)?;
        let paths = self.paths(scope)?;
        let mut ledger = InstallLedger::load(&paths.ledger_file())?;
        if ledger.get(name).is_none() {
            return Err(SkillError::Refused(format!(
                "`{name}` was not installed by LocalPilot in the {} scope; refusing to remove \
                 hand-authored or checked-in content",
                scope.label()
            )));
        }

        self.gate(
            out,
            approval,
            scope,
            &format!(
                "Remove installed skill `{name}` from {} ({}).",
                scope.label(),
                paths.skills_dir().join(name).display()
            ),
        )?;

        delete_installed(&paths.skills_dir(), &mut ledger, name)?;
        line(out, &format!("removed installed skill `{name}`"))
    }

    // --- internals --------------------------------------------------------

    /// Fetch `url` into a staging directory, enforce snapshot bounds, and rename it
    /// into the cache slot for `id`. On any failure the staging dir is removed and
    /// the cache slot is untouched.
    fn fetch_snapshot_into_cache(
        &self,
        paths: &ScopePaths,
        id: &str,
        url: &str,
    ) -> Result<Snapshot, SkillError> {
        let repos = paths.repos_dir();
        std::fs::create_dir_all(&repos).map_err(|source| SkillError::Io {
            path: repos.display().to_string(),
            source,
        })?;
        let staging = repos.join(format!(".staging-{id}"));
        let _ = std::fs::remove_dir_all(&staging);
        let result = (|| {
            let snapshot = self.fetcher.fetch(url, &staging)?;
            ensure_snapshot_within_bounds(&staging)?;
            // Validate the catalog before accepting the snapshot into the cache.
            read_catalog(&staging)?;
            Ok(snapshot)
        })();
        match result {
            Ok(snapshot) => {
                let final_dir = paths.cache_for(id);
                let _ = std::fs::remove_dir_all(&final_dir);
                std::fs::rename(&staging, &final_dir).map_err(|source| {
                    let _ = std::fs::remove_dir_all(&staging);
                    SkillError::Io {
                        path: final_dir.display().to_string(),
                        source,
                    }
                })?;
                Ok(snapshot)
            }
            Err(err) => {
                let _ = std::fs::remove_dir_all(&staging);
                Err(err)
            }
        }
    }

    /// Refresh one source atomically: fetch into staging, validate, then swap the
    /// cache slot (keeping the old copy until the new one is in place).
    fn refresh_one(
        &self,
        paths: &ScopePaths,
        source: &SkillSource,
    ) -> Result<Snapshot, SkillError> {
        // `fetch_snapshot_into_cache` already stages, validates, and swaps
        // atomically; on failure it leaves the existing cache slot untouched.
        self.fetch_snapshot_into_cache(paths, &source.id, &source.url)
    }

    /// Load every source's cached catalog across the read scope, skipping (with a
    /// note) a source whose cache is missing or unreadable.
    fn load_catalogs(
        &self,
        read: ReadScope,
        out: &mut dyn Write,
    ) -> Result<Vec<SourceCatalog>, SkillError> {
        let mut catalogs = Vec::new();
        for (scope, paths) in self.read_scopes(read) {
            let registry = SourceRegistry::load(&paths.sources_file())?;
            for source in registry.sources() {
                let cache = paths.cache_for(&source.id);
                match read_catalog(&cache) {
                    Ok(catalog) => catalogs.push(SourceCatalog {
                        scope,
                        source: source.clone(),
                        catalog,
                        paths: ScopePaths {
                            base: paths.base.clone(),
                        },
                    }),
                    Err(_) => {
                        line(
                            out,
                            &format!(
                                "note: source `{}` has no usable cache — run `skills repo refresh`",
                                source.id
                            ),
                        )?;
                    }
                }
            }
        }
        Ok(catalogs)
    }
}

/// One source paired with its cached catalog and scope, for discovery/install.
struct SourceCatalog {
    scope: Scope,
    source: SkillSource,
    catalog: Catalog,
    paths: ScopePaths,
}

/// Shorten a commit hash for display.
fn short(commit: &str) -> String {
    commit.chars().take(10).collect()
}

/// Write a line to `out`, mapping an I/O failure into a [`SkillError`].
fn line(out: &mut dyn Write, text: &str) -> Result<(), SkillError> {
    writeln!(out, "{text}").map_err(|source| SkillError::Io {
        path: "<output>".to_string(),
        source,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::fetch::Snapshot;

    /// A fetcher that copies a fixture tree into the destination and returns a
    /// fixed commit — the whole management surface without any network.
    struct FakeFetcher {
        fixture: PathBuf,
        commit: String,
    }

    impl RepoFetcher for FakeFetcher {
        fn fetch(&self, _url: &str, dest: &Path) -> Result<Snapshot, SkillError> {
            copy_tree(&self.fixture, dest);
            Ok(Snapshot {
                commit: self.commit.clone(),
            })
        }
    }

    fn copy_tree(src: &Path, dst: &Path) {
        std::fs::create_dir_all(dst).unwrap();
        for entry in std::fs::read_dir(src).unwrap().flatten() {
            let from = entry.path();
            let to = dst.join(entry.file_name());
            if from.is_dir() {
                copy_tree(&from, &to);
            } else {
                std::fs::copy(&from, &to).unwrap();
            }
        }
    }

    /// Build a fixture repo with a `.localpilot/skills` catalog of the given names.
    fn fixture_repo(root: &Path, names: &[&str]) {
        for name in names {
            let dir = root.join(".localpilot").join("skills").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: does {name}\n---\nBody of {name}.\n"),
            )
            .unwrap();
        }
    }

    struct Ctx {
        _tmp: tempfile::TempDir,
        home: PathBuf,
        project: PathBuf,
        fixture: PathBuf,
    }

    fn ctx(names: &[&str]) -> Ctx {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = tmp.path().join("project");
        let fixture = tmp.path().join("fixture");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        fixture_repo(&fixture, names);
        Ctx {
            _tmp: tmp,
            home,
            project,
            fixture,
        }
    }

    fn manager<'a>(c: &'a Ctx, fetcher: &'a dyn RepoFetcher, now: &'a str) -> SkillsManager<'a> {
        SkillsManager::new(&c.project, Some(&c.home), true, fetcher, now)
    }

    #[test]
    fn add_list_available_install_and_delete_round_trip() {
        let c = ctx(&["alpha", "beta"]);
        let fetcher = FakeFetcher {
            fixture: c.fixture.clone(),
            commit: "c0ffee1234".to_string(),
        };
        let m = manager(&c, &fetcher, "1000");
        let url = "https://github.com/owner/repo";

        let mut buf = Vec::new();
        m.repo_add(Scope::Project, url, Approval::AssumeYes, &mut buf)
            .unwrap();
        assert!(String::from_utf8(buf).unwrap().contains("added source"));

        // Offline available reads the cached catalog.
        let mut buf = Vec::new();
        m.available(ReadScope::Effective, None, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("alpha") && text.contains("beta"), "{text}");
        assert!(text.contains("available"), "{text}");

        // Install one skill; it lands in the project skills dir and is effective.
        let mut buf = Vec::new();
        m.install(
            Scope::Project,
            InstallSpec::Named {
                name: "alpha".to_string(),
                repo: None,
            },
            Approval::AssumeYes,
            &mut buf,
        )
        .unwrap();
        let installed = c
            .project
            .join(".localpilot")
            .join("skills")
            .join("alpha")
            .join("SKILL.md");
        assert!(installed.is_file(), "installed skill missing");

        // available now reports it installed.
        let mut buf = Vec::new();
        m.available(ReadScope::Effective, Some("alpha"), &mut buf)
            .unwrap();
        assert!(String::from_utf8(buf).unwrap().contains("installed"));

        // Delete it.
        let mut buf = Vec::new();
        m.delete(Scope::Project, "alpha", Approval::AssumeYes, &mut buf)
            .unwrap();
        assert!(!c
            .project
            .join(".localpilot")
            .join("skills")
            .join("alpha")
            .exists());
    }

    #[test]
    fn re_adding_a_source_is_refused() {
        let c = ctx(&["alpha"]);
        let fetcher = FakeFetcher {
            fixture: c.fixture.clone(),
            commit: "abc".to_string(),
        };
        let m = manager(&c, &fetcher, "1");
        let mut buf = Vec::new();
        m.repo_add(
            Scope::Project,
            "https://github.com/o/r",
            Approval::AssumeYes,
            &mut buf,
        )
        .unwrap();
        let err = m
            .repo_add(
                Scope::Project,
                "https://github.com/o/r.git",
                Approval::AssumeYes,
                &mut Vec::new(),
            )
            .unwrap_err();
        assert!(matches!(err, SkillError::Conflict(_)), "got {err:?}");
    }

    #[test]
    fn install_all_is_all_or_nothing() {
        let c = ctx(&["alpha", "beta"]);
        let fetcher = FakeFetcher {
            fixture: c.fixture.clone(),
            commit: "abc".to_string(),
        };
        let m = manager(&c, &fetcher, "1");
        m.repo_add(
            Scope::Project,
            "https://github.com/o/r",
            Approval::AssumeYes,
            &mut Vec::new(),
        )
        .unwrap();
        let id = source_id("https://github.com/o/r");

        // Pre-create a conflicting `alpha` so the bulk install must abort wholesale.
        let skills = c.project.join(".localpilot").join("skills");
        std::fs::create_dir_all(skills.join("alpha")).unwrap();
        std::fs::write(
            skills.join("alpha").join("SKILL.md"),
            "---\nname: alpha\ndescription: hand\n---\nb\n",
        )
        .unwrap();

        let err = m
            .install(
                Scope::Project,
                InstallSpec::All { repo: id },
                Approval::AssumeYes,
                &mut Vec::new(),
            )
            .unwrap_err();
        assert!(matches!(err, SkillError::Conflict(_)), "got {err:?}");
        // beta must NOT have been installed (all-or-nothing).
        assert!(!skills.join("beta").exists(), "partial install leaked");
    }

    #[test]
    fn untrusted_project_mutation_is_refused_but_global_works() {
        let c = ctx(&["alpha"]);
        let fetcher = FakeFetcher {
            fixture: c.fixture.clone(),
            commit: "abc".to_string(),
        };
        // Untrusted workspace.
        let m = SkillsManager::new(&c.project, Some(&c.home), false, &fetcher, "1");
        let err = m
            .repo_add(
                Scope::Project,
                "https://github.com/o/r",
                Approval::AssumeYes,
                &mut Vec::new(),
            )
            .unwrap_err();
        assert!(matches!(err, SkillError::Refused(_)), "got {err:?}");
        // The same operation in the global scope is allowed (trust gates project).
        m.repo_add(
            Scope::Global,
            "https://github.com/o/r",
            Approval::AssumeYes,
            &mut Vec::new(),
        )
        .unwrap();
    }

    #[test]
    fn non_interactive_without_yes_is_refused_after_disclosure() {
        let c = ctx(&["alpha"]);
        let fetcher = FakeFetcher {
            fixture: c.fixture.clone(),
            commit: "abc".to_string(),
        };
        let m = manager(&c, &fetcher, "1");
        let mut buf = Vec::new();
        let err = m
            .repo_add(
                Scope::Project,
                "https://github.com/o/r",
                Approval::NonInteractive,
                &mut buf,
            )
            .unwrap_err();
        assert!(matches!(err, SkillError::Refused(_)), "got {err:?}");
        // The impact was still disclosed before the refusal.
        assert!(String::from_utf8(buf).unwrap().contains("Fetch (network)"));
    }

    #[test]
    fn failed_refresh_keeps_the_previous_cache() {
        let c = ctx(&["alpha"]);
        let good = FakeFetcher {
            fixture: c.fixture.clone(),
            commit: "good".to_string(),
        };
        let m = manager(&c, &good, "1");
        m.repo_add(
            Scope::Project,
            "https://github.com/o/r",
            Approval::AssumeYes,
            &mut Vec::new(),
        )
        .unwrap();
        let id = source_id("https://github.com/o/r");
        let cache = c.project.join(".localpilot").join("skill-repos").join(&id);
        assert!(
            cache
                .join(".localpilot")
                .join("skills")
                .join("alpha")
                .is_file()
                || cache
                    .join(".localpilot")
                    .join("skills")
                    .join("alpha")
                    .join("SKILL.md")
                    .is_file()
        );

        // A fetcher that always fails: the previous cache must survive.
        struct Failing;
        impl RepoFetcher for Failing {
            fn fetch(&self, _url: &str, _dest: &Path) -> Result<Snapshot, SkillError> {
                Err(SkillError::Fetch("network down".to_string()))
            }
        }
        let m2 = SkillsManager::new(&c.project, Some(&c.home), true, &Failing, "2");
        let err = m2
            .repo_refresh(Scope::Project, None, Approval::AssumeYes, &mut Vec::new())
            .unwrap_err();
        assert!(matches!(err, SkillError::Fetch(_)), "got {err:?}");
        assert!(
            cache
                .join(".localpilot")
                .join("skills")
                .join("alpha")
                .join("SKILL.md")
                .is_file(),
            "previous cache was lost on a failed refresh"
        );
    }

    #[test]
    fn project_install_shadows_a_global_install_of_the_same_name() {
        let c = ctx(&["alpha"]);
        let fetcher = FakeFetcher {
            fixture: c.fixture.clone(),
            commit: "abc".to_string(),
        };
        let m = manager(&c, &fetcher, "1");
        // Register the source globally and install `alpha` into the global scope.
        m.repo_add(
            Scope::Global,
            "https://github.com/o/r",
            Approval::AssumeYes,
            &mut Vec::new(),
        )
        .unwrap();
        m.install(
            Scope::Global,
            InstallSpec::Named {
                name: "alpha".to_string(),
                repo: None,
            },
            Approval::AssumeYes,
            &mut Vec::new(),
        )
        .unwrap();
        // A project install draws from the effective (project+global) sources; with
        // only the global source registered, `alpha` resolves unambiguously and
        // lands in the project scope, deliberately shadowing the global copy.
        m.install(
            Scope::Project,
            InstallSpec::Named {
                name: "alpha".to_string(),
                repo: None,
            },
            Approval::AssumeYes,
            &mut Vec::new(),
        )
        .unwrap();
        // Both scope directories hold their own copy — the project shadows global.
        assert!(c
            .home
            .join(".localpilot")
            .join("skills")
            .join("alpha")
            .exists());
        assert!(c
            .project
            .join(".localpilot")
            .join("skills")
            .join("alpha")
            .exists());
    }
}
