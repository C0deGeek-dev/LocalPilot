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

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::catalog::{read_catalog, Catalog};
use crate::discovery::{DiscoveredSkill, MatchState};
use crate::error::SkillError;
use crate::fetch::{ensure_snapshot_within_bounds, RepoFetcher, Snapshot};
use crate::install::{delete_installed, install_package, InstallLedger, Provenance};
use crate::loader::{discovery_roots, global_only_roots, SkillSet};
use crate::source::{normalize_url, source_id, SkillSource, SourceRegistry};
use crate::update::{self, Change, Fault, Outcome};

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

/// Semantic styling for the human-facing skill catalog. The manager owns the
/// layout but not terminal policy: CLI callers may add color, while captured
/// output and library callers use [`PlainSkillCatalogStyle`].
pub trait SkillCatalogStyle {
    /// Style a skill name without changing its text.
    fn name(&self, value: &str) -> String;
    /// Style an availability label without changing its text.
    fn state(&self, state: MatchState) -> String;
}

/// A no-escape catalog style for pipes, tests, and interactive transcripts.
#[derive(Debug, Default, Clone, Copy)]
pub struct PlainSkillCatalogStyle;

impl SkillCatalogStyle for PlainSkillCatalogStyle {
    fn name(&self, value: &str) -> String {
        value.to_string()
    }

    fn state(&self, state: MatchState) -> String {
        state.label().to_string()
    }
}

/// What an install targets.
#[derive(Debug, Clone)]
pub enum InstallSpec {
    /// One named skill, optionally pinned to a source id (`--repo`).
    Named { name: String, repo: Option<String> },
    /// Every package of one source (`--all --repo <id>`).
    All { repo: String },
}

/// What `update` targets.
#[derive(Debug, Clone)]
pub enum UpdateTarget {
    /// One managed skill by name.
    Named(String),
    /// Every managed skill in the scope (`--all`).
    All,
}

/// A managed (LocalPilot-installed) skill as its ledger and source see it,
/// offline: what `skills list` and `doctor` report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedSkill {
    pub name: String,
    pub scope: Scope,
    pub source_id: String,
    /// The commit the installed copy came from.
    pub installed: String,
    /// The source's last refreshed commit, when the source is still registered.
    pub cached: Option<String>,
    /// The installed directory is gone while its ledger entry remains.
    pub missing: bool,
}

impl ManagedSkill {
    /// `(installed, cached)` when a refresh has moved the source past the
    /// installed copy.
    #[must_use]
    pub fn stale(&self) -> Option<(&str, &str)> {
        match &self.cached {
            Some(head) if *head != self.installed => Some((&self.installed, head)),
            _ => None,
        }
    }
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
    fn repos(&self) -> PathBuf {
        self.repos_dir()
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
    /// Where a test stops or breaks a multi-step change, as a crash would.
    fault: Option<Fault>,
    /// How long a mutation waits for another process's change to the same
    /// scope before refusing.
    lock_wait: Duration,
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
            fault: None,
            lock_wait: Duration::from_secs(10),
        }
    }

    /// The same manager waiting at most `wait` for the scope lock.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_lock_wait(mut self, wait: Duration) -> Self {
        self.lock_wait = wait;
        self
    }

    /// Run a mutation of `scope` holding that scope's cross-process lock,
    /// from its recovery to its end, network included. The lock is an OS
    /// advisory lock on `<scope>/skills.lock`, released by the OS if the
    /// holder dies, so there is never a stale lock to guess about; the file
    /// itself is never removed.
    fn with_scope_lock<T>(
        &self,
        scope: Scope,
        paths: &ScopePaths,
        f: impl FnOnce() -> Result<T, SkillError>,
    ) -> Result<T, SkillError> {
        std::fs::create_dir_all(&paths.base).map_err(|source| SkillError::Io {
            path: paths.base.display().to_string(),
            source,
        })?;
        let path = paths.base.join("skills.lock");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| SkillError::Io {
                path: path.display().to_string(),
                source,
            })?;
        let mut lock = fd_lock::RwLock::new(file);
        let deadline = Instant::now() + self.lock_wait;
        loop {
            match lock.try_write() {
                Ok(_held) => return f(),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(source) => {
                    return Err(SkillError::Io {
                        path: path.display().to_string(),
                        source,
                    })
                }
            }
            if Instant::now() >= deadline {
                return Err(SkillError::Refused(format!(
                    "another localpilot skills command is changing the {} scope; try again",
                    scope.label()
                )));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The same manager with a test fault armed.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_fault(mut self, fault: Fault) -> Self {
        self.fault = Some(fault);
        self
    }

    /// Finish or undo whatever a crash interrupted in this scope: a cache swap,
    /// then a skills update. Every mutation runs this first.
    fn recover(&self, paths: &ScopePaths, out: &mut dyn Write) -> Result<(), SkillError> {
        for done in update::recover_caches(&paths.repos())? {
            line(out, &format!("recovered: {done}"))?;
        }
        if let Some(done) = update::recover_update(&paths.base)? {
            line(out, &format!("recovered: {done}"))?;
        }
        Ok(())
    }

    /// The scopes, of those a read covers, with an interrupted change waiting
    /// for recovery. Read-only commands report these and change nothing.
    #[must_use]
    pub fn recovery_pending(&self, read: ReadScope) -> Vec<Scope> {
        self.read_scopes(read)
            .into_iter()
            .filter(|(_, p)| {
                update::update_pending(&p.base) || update::cache_swap_pending(&p.repos())
            })
            .map(|(s, _)| s)
            .collect()
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
        // An untrusted workspace contributes no project skills, sources, or
        // catalogs to an effective read, matching what the file-discovery half
        // (`discovery_roots`) already does — so reads and mutations answer to the
        // same folder-trust rule.
        if matches!(read, ReadScope::Effective) && self.trusted {
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
                "workspace is not trusted; run `localpilot trust add` for this folder, or retry \
                 with `--global`"
                    .to_string(),
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
        let lock_paths = self.paths(scope)?;
        self.with_scope_lock(scope, &lock_paths, || {
        self.ensure_mutable(scope)?;
        let normalized = normalize_url(url)?;
        let id = source_id(&normalized);
        let paths = self.paths(scope)?;
        self.recover(&paths, out)?;
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

        let snapshot = self.fetch_snapshot_into_cache(&paths, &id, &normalized, None)?;
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
        })
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
        let lock_paths = self.paths(scope)?;
        self.with_scope_lock(scope, &lock_paths, || {
            self.ensure_mutable(scope)?;
            let paths = self.paths(scope)?;
            self.recover(&paths, out)?;
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
                match self.refresh_one(&paths, source, &mut registry) {
                    Ok(snapshot) => {
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
        })
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
        let lock_paths = self.paths(scope)?;
        self.with_scope_lock(scope, &lock_paths, || {
            self.ensure_mutable(scope)?;
            let paths = self.paths(scope)?;
            self.recover(&paths, out)?;
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
        })
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
        self.available_styled(read, query, &PlainSkillCatalogStyle, out)
    }

    /// Render cached catalog entries with an injected semantic style. Entry
    /// boundaries and complete descriptions remain identical in every style.
    pub fn available_styled(
        &self,
        read: ReadScope,
        query: Option<&str>,
        style: &dyn SkillCatalogStyle,
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
                let state = if ledger.get(&package.name).is_some() {
                    MatchState::Installed
                } else {
                    MatchState::Available
                };
                let ambiguous = if name_counts.get(package.name.as_str()).copied().unwrap_or(0) > 1
                {
                    " [ambiguous: several sources — use --repo <id>]"
                } else {
                    ""
                };
                if shown > 0 {
                    line(out, "")?;
                }
                shown += 1;
                line(
                    out,
                    &format!(
                        "- {} [{}] ({}, {} @ {}){}",
                        style.name(&package.name),
                        style.state(state),
                        entry.scope.label(),
                        entry.source.id,
                        short(&entry.source.commit),
                        ambiguous,
                    ),
                )?;
                // Unlike the lean `skills list` and model-tool indexes, this is
                // the install-decision surface: the complete trigger-bearing
                // description is deliberate and must not be capped.
                line(out, &format!("  {}", package.description))?;
            }
        }
        if shown == 0 {
            line(out, "no matching skills in any cached source")?;
        }
        Ok(())
    }

    /// Structured, read-only discovery of the local view: the effective installed
    /// skills (the #39 resolution) plus the packages available from registered
    /// sources, each classified [`MatchState::Installed`] or
    /// [`MatchState::Available`]. No network and no output — this is the local
    /// data the discovery lane ranks and the review surface consumes; an installed
    /// skill shadows an available package of the same name (LocalHub#41).
    ///
    /// # Errors
    /// Returns an error only if reading a registry or a cached catalog fails.
    pub fn local_discovery(&self, read: ReadScope) -> Result<Vec<DiscoveredSkill>, SkillError> {
        // Installed = present in the effective skill catalog, resolved with the
        // same precedence and trust gate the loader uses (#39).
        let roots = match read {
            ReadScope::Effective => discovery_roots(self.project_root, self.home, self.trusted),
            ReadScope::GlobalOnly => global_only_roots(self.home),
        };
        let effective = SkillSet::resolve(&roots)?;
        let mut by_name: BTreeMap<String, DiscoveredSkill> = BTreeMap::new();
        for name in effective.names() {
            if let Some(skill) = effective.by_name(name) {
                by_name.insert(
                    skill.manifest.name.clone(),
                    DiscoveredSkill {
                        name: skill.manifest.name.clone(),
                        description: skill.manifest.description.clone(),
                        state: MatchState::Installed,
                        repo_url: None,
                        source_id: None,
                        commit: None,
                        catalog_root: None,
                        source_path: None,
                        discoverable: skill.manifest.invocation.is_discoverable(),
                    },
                );
            }
        }
        // Available = in a registered source's cached catalog but not installed.
        let (catalogs, _unreadable) = self.source_catalogs(read)?;
        for entry in &catalogs {
            for package in &entry.catalog.packages {
                by_name.entry(package.name.clone()).or_insert_with(|| {
                    DiscoveredSkill::available(
                        package,
                        &entry.source.id,
                        &entry.source.url,
                        &entry.source.commit,
                        &entry.catalog.root_label,
                    )
                });
            }
        }
        Ok(by_name.into_values().collect())
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
        let lock_paths = self.paths(scope)?;
        self.with_scope_lock(scope, &lock_paths, || {
            self.ensure_mutable(scope)?;
            self.recover(&self.paths(scope)?, out)?;
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
        })
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
        let lock_paths = self.paths(scope)?;
        self.with_scope_lock(scope, &lock_paths, || {
            self.ensure_mutable(scope)?;
            let paths = self.paths(scope)?;
            self.recover(&paths, out)?;
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
        })
    }

    /// Update managed skills in `scope` to their sources' current commits:
    /// refresh each owning source once, then replace every changed package of
    /// that source in one transaction, or none of them. A package whose
    /// source no longer offers it at the same path under the same name is
    /// refused, never substituted.
    ///
    /// # Errors
    /// Refuses an untrusted/unapproved mutation or an unmanaged name; reports
    /// each failed source and fails when any did.
    pub fn update(
        &self,
        scope: Scope,
        target: UpdateTarget,
        approval: Approval<'_>,
        out: &mut dyn Write,
    ) -> Result<(), SkillError> {
        self.ensure_mutable(scope)?;
        let lock_paths = self.paths(scope)?;
        self.with_scope_lock(scope, &lock_paths, || {
            self.ensure_mutable(scope)?;
            let paths = self.paths(scope)?;
            self.recover(&paths, out)?;
            let ledger = InstallLedger::load(&paths.ledger_file())?;
            let entries: Vec<Provenance> = match &target {
                UpdateTarget::Named(name) => vec![ledger.get(name).cloned().ok_or_else(|| {
                    SkillError::Refused(format!(
                    "`{name}` was not installed by LocalPilot in the {} scope; nothing to update",
                    scope.label()
                ))
                })?],
                UpdateTarget::All => ledger.entries().to_vec(),
            };
            if entries.is_empty() {
                return Err(SkillError::NotFound(format!(
                    "no managed skills in the {} scope",
                    scope.label()
                )));
            }
            let mut by_source: BTreeMap<String, Vec<Provenance>> = BTreeMap::new();
            for e in entries {
                by_source.entry(e.source_id.clone()).or_default().push(e);
            }
            let names: Vec<&str> = by_source
                .values()
                .flatten()
                .map(|e| e.name.as_str())
                .collect();
            self.gate(
                out,
                approval,
                scope,
                &format!(
                    "Update {} managed skill(s) [{}] in the {} scope from {} source(s) (network). \
                 Each changed installed copy is replaced; edits made to it are lost.",
                    names.len(),
                    names.join(", "),
                    scope.label(),
                    by_source.len()
                ),
            )?;

            let mut registry = SourceRegistry::load(&paths.sources_file())?;
            let mut failures = 0usize;
            for (source_id, wanted) in &by_source {
                if let Err(err) =
                    self.update_source(scope, &paths, &mut registry, source_id, wanted, out)
                {
                    failures += 1;
                    line(out, &format!("could not update from `{source_id}`: {err}"))?;
                }
            }
            if failures > 0 {
                return Err(SkillError::Fetch(format!(
                    "{failures} of {} source(s) failed to update; their skills are unchanged",
                    by_source.len()
                )));
            }
            Ok(())
        })
    }

    fn update_source(
        &self,
        scope: Scope,
        paths: &ScopePaths,
        registry: &mut SourceRegistry,
        source_id: &str,
        wanted: &[Provenance],
        out: &mut dyn Write,
    ) -> Result<(), SkillError> {
        if let Some(source) = registry.find(source_id).cloned() {
            let snapshot = self.refresh_one(paths, &source, registry)?;
            let catalog = read_catalog(&paths.cache_for(&source.id))?;
            return self.replace_from(paths, &source, &snapshot.commit, &catalog, wanted, out);
        }
        // A project install may come from a global source (install draws on
        // the effective sources). A project command does not change the
        // global scope, so it uses that source's current cache, under the
        // global lock so a global refresh cannot swap it mid-copy. The lock
        // order is always project, then global.
        let global = match (scope, self.paths(Scope::Global)) {
            (Scope::Project, Ok(global)) => global,
            _ => return Err(no_longer_registered(source_id)),
        };
        self.with_scope_lock(Scope::Global, &global, || {
            let registry = SourceRegistry::load(&global.sources_file())?;
            let source = registry
                .find(source_id)
                .cloned()
                .ok_or_else(|| no_longer_registered(source_id))?;
            line(
                out,
                &format!(
                    "`{}` is a global source; using its cache @ {} (refresh it with \
                     `localpilot skills repo refresh -g`)",
                    source.id,
                    short(&source.commit)
                ),
            )?;
            let catalog = read_catalog(&global.cache_for(&source.id))?;
            self.replace_from(paths, &source, &source.commit, &catalog, wanted, out)
        })
    }

    /// Replace each of `wanted` from `catalog` at `commit` into `paths`, as one
    /// transaction; unchanged present ones are reported up to date.
    fn replace_from(
        &self,
        paths: &ScopePaths,
        source: &SkillSource,
        commit: &str,
        catalog: &Catalog,
        wanted: &[Provenance],
        out: &mut dyn Write,
    ) -> Result<(), SkillError> {
        let snapshot = Snapshot {
            commit: commit.to_owned(),
        };
        let mut changes = Vec::new();
        let mut current = Vec::new();
        for e in wanted {
            let package = catalog
                .packages
                .iter()
                .find(|p| p.source_path == e.source_path && p.name == e.name)
                .ok_or_else(|| {
                    SkillError::Rejected(format!(
                        "`{}`: its path {} is gone from `{}` @ {}; reinstall it explicitly",
                        e.name,
                        e.source_path,
                        source.id,
                        short(&snapshot.commit)
                    ))
                })?;
            let present = paths.skills_dir().join(&e.name).exists();
            if e.commit == snapshot.commit && present {
                current.push(e.name.clone());
                continue;
            }
            let mut provenance = e.clone();
            provenance.commit = snapshot.commit.clone();
            provenance.installed_at = self.now.to_string();
            changes.push((
                e.commit.clone(),
                present,
                Change {
                    package,
                    provenance,
                },
            ));
        }
        for name in &current {
            line(
                out,
                &format!("`{name}` up to date @ {}", short(&snapshot.commit)),
            )?;
        }
        if changes.is_empty() {
            return Ok(());
        }
        let staged: Vec<Change<'_>> = changes
            .iter()
            .map(|(_, _, c)| Change {
                package: c.package,
                provenance: c.provenance.clone(),
            })
            .collect();
        match update::replace(
            &paths.base,
            &paths.skills_dir(),
            &paths.ledger_file(),
            &staged,
            self.fault,
        )? {
            Outcome::Done => {}
            Outcome::Stopped => return Err(stopped()),
        }
        for (old, present, c) in &changes {
            let how = if *present { "updated" } else { "reinstalled" };
            line(
                out,
                &format!(
                    "{how} `{}`: {} -> {}",
                    c.provenance.name,
                    short(old),
                    short(&snapshot.commit)
                ),
            )?;
        }
        Ok(())
    }

    /// Every managed skill across the read scope with its staleness (the
    /// ledger commit against the source's last refreshed commit) and whether
    /// its directory is missing. Offline.
    ///
    /// # Errors
    /// An unreadable ledger or registry.
    pub fn managed(&self, read: ReadScope) -> Result<Vec<ManagedSkill>, SkillError> {
        let mut out = Vec::new();
        for (scope, paths) in self.read_scopes(read) {
            let registry = SourceRegistry::load(&paths.sources_file())?;
            // A project install may come from a global source.
            let global = match (scope, self.paths(Scope::Global)) {
                (Scope::Project, Ok(g)) => Some(SourceRegistry::load(&g.sources_file())?),
                _ => None,
            };
            let ledger = InstallLedger::load(&paths.ledger_file())?;
            for e in ledger.entries() {
                let source = registry
                    .find(&e.source_id)
                    .or_else(|| global.as_ref().and_then(|g| g.find(&e.source_id)));
                out.push(ManagedSkill {
                    name: e.name.clone(),
                    scope,
                    source_id: e.source_id.clone(),
                    installed: e.commit.clone(),
                    cached: source.map(|s| s.commit.clone()),
                    missing: !paths.skills_dir().join(&e.name).exists(),
                });
            }
        }
        Ok(out)
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
        registry: Option<&mut SourceRegistry>,
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
                // The old cache is set aside, never deleted, until the new one
                // is in place and the registry records its commit.
                let swapped = update::swap_cache(
                    &repos,
                    id,
                    &staging,
                    &snapshot.commit,
                    registry,
                    self.fault,
                );
                match swapped {
                    Ok(Outcome::Done) => Ok(snapshot),
                    Ok(Outcome::Stopped) => Err(stopped()),
                    Err(err) => {
                        let _ = std::fs::remove_dir_all(&staging);
                        Err(err)
                    }
                }
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
        registry: &mut SourceRegistry,
    ) -> Result<Snapshot, SkillError> {
        // Stages, validates, swaps recoverably and records the new commit; on
        // failure the old cache and commit stay.
        self.fetch_snapshot_into_cache(paths, &source.id, &source.url, Some(registry))
    }

    /// Gather every source's cached catalog across the read scope. A source whose
    /// cache is missing or unreadable is not fatal; its id is returned in the
    /// second vector so a caller can surface it. No output — the shared plumbing
    /// behind both the text `available` listing and structured `local_discovery`.
    fn source_catalogs(
        &self,
        read: ReadScope,
    ) -> Result<(Vec<SourceCatalog>, Vec<String>), SkillError> {
        let mut catalogs = Vec::new();
        let mut unreadable = Vec::new();
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
                    Err(_) => unreadable.push(source.id.clone()),
                }
            }
        }
        Ok((catalogs, unreadable))
    }

    /// Load every source's cached catalog across the read scope, noting (on `out`)
    /// a source whose cache is missing or unreadable.
    fn load_catalogs(
        &self,
        read: ReadScope,
        out: &mut dyn Write,
    ) -> Result<Vec<SourceCatalog>, SkillError> {
        let (catalogs, unreadable) = self.source_catalogs(read)?;
        for id in unreadable {
            line(
                out,
                &format!("note: source `{id}` has no usable cache — run `skills repo refresh`"),
            )?;
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

fn no_longer_registered(source_id: &str) -> SkillError {
    SkillError::NotFound(format!(
        "source `{source_id}` is no longer registered; reinstall its skills explicitly"
    ))
}

/// What a test fault that stops a change midway reports, as a crash would.
fn stopped() -> SkillError {
    SkillError::Io {
        path: String::new(),
        source: std::io::Error::other("stopped by a test fault"),
    }
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

    struct MarkerStyle;

    impl SkillCatalogStyle for MarkerStyle {
        fn name(&self, value: &str) -> String {
            format!("<name>{value}</name>")
        }

        fn state(&self, state: MatchState) -> String {
            format!("<state>{}</state>", state.label())
        }
    }

    #[test]
    fn add_list_available_install_and_delete_round_trip() {
        let c = ctx(&["alpha", "beta"]);
        let long_description = format!("{}final detail", "beta decision detail ".repeat(40));
        std::fs::write(
            c.fixture
                .join(".localpilot")
                .join("skills")
                .join("beta")
                .join("SKILL.md"),
            format!("---\nname: beta\ndescription: {long_description}\n---\nBody of beta.\n"),
        )
        .unwrap();
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
        assert!(
            text.contains(&long_description),
            "decision reports must not truncate descriptions: {text}"
        );
        assert!(
            text.contains("  does alpha\n\n- beta [available]"),
            "entries have a full, indented description and a blank-line boundary: {text}"
        );
        let mut styled = Vec::new();
        m.available_styled(
            ReadScope::Effective,
            Some("alpha"),
            &MarkerStyle,
            &mut styled,
        )
        .unwrap();
        let styled = String::from_utf8(styled).unwrap();
        assert!(
            styled.contains("- <name>alpha</name> [<state>available</state>]")
                && styled.contains("\n  does alpha\n"),
            "only identity fields receive semantic styling: {styled}"
        );

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
        let SkillError::Refused(message) = &err else {
            panic!("got {err:?}");
        };
        // The refusal names both remedies so the user is not stuck.
        assert!(message.contains("localpilot trust add"), "got {message:?}");
        assert!(message.contains("--global"), "got {message:?}");
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
    fn untrusted_effective_read_excludes_project_sources_but_keeps_global() {
        let c = ctx(&["alpha"]);
        let fetcher = FakeFetcher {
            fixture: c.fixture.clone(),
            commit: "abc".to_string(),
        };
        // Trusted: register a project source and a global source.
        let trusted = SkillsManager::new(&c.project, Some(&c.home), true, &fetcher, "1");
        trusted
            .repo_add(
                Scope::Project,
                "https://github.com/o/project",
                Approval::AssumeYes,
                &mut Vec::new(),
            )
            .unwrap();
        trusted
            .repo_add(
                Scope::Global,
                "https://github.com/o/global",
                Approval::AssumeYes,
                &mut Vec::new(),
            )
            .unwrap();

        // Untrusted: an effective read sees only the global source.
        let untrusted = SkillsManager::new(&c.project, Some(&c.home), false, &fetcher, "1");
        let mut effective = Vec::new();
        untrusted
            .repo_list(ReadScope::Effective, &mut effective)
            .unwrap();
        let effective = String::from_utf8(effective).unwrap();
        assert!(effective.contains("o/global"), "got {effective:?}");
        assert!(!effective.contains("o/project"), "got {effective:?}");

        // A trusted effective read sees both.
        let mut both = Vec::new();
        trusted.repo_list(ReadScope::Effective, &mut both).unwrap();
        let both = String::from_utf8(both).unwrap();
        assert!(both.contains("o/project") && both.contains("o/global"));
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
    fn local_discovery_classifies_installed_and_available_and_installed_shadows() {
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
        // Install `alpha` into the project so it enters the effective catalog.
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

        let found = m.local_discovery(ReadScope::Effective).unwrap();
        let by = |name: &str| found.iter().find(|d| d.name == name).cloned();
        // `alpha` is installed (in the effective catalog); `beta` is only available.
        assert_eq!(by("alpha").unwrap().state, MatchState::Installed);
        assert_eq!(by("beta").unwrap().state, MatchState::Available);
        // An installed skill carries no repository fields; an available one does.
        assert!(by("alpha").unwrap().repo_url.is_none());
        let beta = by("beta").unwrap();
        assert_eq!(beta.repo_url.as_deref(), Some("https://github.com/o/r"));
        assert_eq!(beta.commit.as_deref(), Some("abc"));
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

#[cfg(test)]
mod update_tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::fetch::Snapshot;
    use std::sync::Mutex;

    /// A fetcher whose fixture tree and commit the test moves between calls.
    struct MovingFetcher {
        fixture: PathBuf,
        commit: Mutex<String>,
    }

    impl RepoFetcher for MovingFetcher {
        fn fetch(&self, _url: &str, dest: &Path) -> Result<Snapshot, SkillError> {
            copy(&self.fixture, dest);
            Ok(Snapshot {
                commit: self.commit.lock().unwrap().clone(),
            })
        }
    }

    fn copy(src: &Path, dst: &Path) {
        std::fs::create_dir_all(dst).unwrap();
        for entry in std::fs::read_dir(src).unwrap().flatten() {
            let to = dst.join(entry.file_name());
            if entry.path().is_dir() {
                copy(&entry.path(), &to);
            } else {
                std::fs::copy(entry.path(), &to).unwrap();
            }
        }
    }

    fn write_skill(fixture: &Path, name: &str, body: &str) {
        let dir = fixture.join(".localpilot").join("skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: does {name}\n---\n{body}\n"),
        )
        .unwrap();
    }

    struct Ctx {
        _tmp: tempfile::TempDir,
        home: PathBuf,
        project: PathBuf,
        fetcher: MovingFetcher,
    }

    const URL: &str = "https://github.com/owner/repo";

    /// alpha and beta installed in the project from commit `aaaaaaa1`.
    fn installed() -> Ctx {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = tmp.path().join("project");
        let fixture = tmp.path().join("fixture");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        write_skill(&fixture, "alpha", "alpha A");
        write_skill(&fixture, "beta", "beta A");
        let c = Ctx {
            _tmp: tmp,
            home,
            project,
            fetcher: MovingFetcher {
                fixture,
                commit: Mutex::new("aaaaaaa1".into()),
            },
        };
        let m = c.manager();
        let mut out = Vec::new();
        m.repo_add(Scope::Project, URL, Approval::AssumeYes, &mut out)
            .unwrap();
        m.install(
            Scope::Project,
            InstallSpec::All {
                repo: source_id(&normalize_url(URL).unwrap()),
            },
            Approval::AssumeYes,
            &mut out,
        )
        .unwrap();
        c
    }

    impl Ctx {
        fn manager(&self) -> SkillsManager<'_> {
            SkillsManager::new(&self.project, Some(&self.home), true, &self.fetcher, "2000")
        }
        /// The source moves on to commit `bbbbbbb2` with new bodies.
        fn move_source(&self) {
            write_skill(&self.fetcher.fixture, "alpha", "alpha B");
            write_skill(&self.fetcher.fixture, "beta", "beta B");
            *self.fetcher.commit.lock().unwrap() = "bbbbbbb2".into();
        }
        fn base(&self) -> PathBuf {
            self.project.join(".localpilot")
        }
        fn body(&self, name: &str) -> Option<String> {
            std::fs::read_to_string(self.base().join("skills").join(name).join("SKILL.md")).ok()
        }
        fn ledger(&self) -> String {
            std::fs::read_to_string(self.base().join("installed-skills.toml")).unwrap()
        }
        fn registry_commit(&self) -> String {
            let r = SourceRegistry::load(&self.base().join("skill-sources.toml")).unwrap();
            r.sources()[0].commit.clone()
        }
        fn cached_body(&self, name: &str) -> String {
            let id = source_id(&normalize_url(URL).unwrap());
            std::fs::read_to_string(
                self.base()
                    .join("skill-repos")
                    .join(id)
                    .join(".localpilot")
                    .join("skills")
                    .join(name)
                    .join("SKILL.md"),
            )
            .unwrap()
        }
        /// Everything an update leaves behind in the scope's working area.
        fn leftovers(&self) -> Vec<String> {
            std::fs::read_dir(update::update_dir(&self.base()))
                .map(|d| {
                    d.flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default()
        }
        fn update(&self, fault: Option<Fault>) -> (Result<(), SkillError>, String) {
            let mut m = self.manager();
            if let Some(f) = fault {
                m = m.with_fault(f);
            }
            let mut out = Vec::new();
            let r = m.update(
                Scope::Project,
                UpdateTarget::All,
                Approval::AssumeYes,
                &mut out,
            );
            (r, String::from_utf8(out).unwrap())
        }
        /// Any mutation recovers first; an unknown source makes it do nothing else.
        fn recover(&self) -> String {
            let mut out = Vec::new();
            let _ = self.manager().repo_delete(
                Scope::Project,
                "no-such-source",
                Approval::AssumeYes,
                &mut out,
            );
            String::from_utf8(out).unwrap()
        }
        fn effective(&self) -> SkillSet {
            SkillSet::resolve(&discovery_roots(&self.project, Some(&self.home), true)).unwrap()
        }
    }

    #[test]
    fn a_refreshed_source_shows_stale_and_update_installs_the_new_commit() {
        let c = installed();
        c.move_source();
        let mut out = Vec::new();
        c.manager()
            .repo_refresh(Scope::Project, None, Approval::AssumeYes, &mut out)
            .unwrap();
        let managed = c.manager().managed(ReadScope::Effective).unwrap();
        assert_eq!(managed.len(), 2);
        for m in &managed {
            assert_eq!(m.stale(), Some(("aaaaaaa1", "bbbbbbb2")), "{m:?}");
            assert!(!m.missing);
        }
        let (r, text) = c.update(None);
        r.unwrap();
        assert!(
            text.contains("updated `alpha`: aaaaaaa1 -> bbbbbbb2"),
            "{text}"
        );
        assert!(
            text.contains("updated `beta`: aaaaaaa1 -> bbbbbbb2"),
            "{text}"
        );
        assert!(c.body("alpha").unwrap().contains("alpha B"));
        assert!(c.ledger().contains("bbbbbbb2") && !c.ledger().contains("aaaaaaa1"));
        assert!(c
            .manager()
            .managed(ReadScope::Effective)
            .unwrap()
            .iter()
            .all(|m| m.stale().is_none()));
        assert!(c.leftovers().is_empty(), "{:?}", c.leftovers());
        // Nothing moved: an update is a no-op that says so.
        let (r, text) = c.update(None);
        r.unwrap();
        assert!(text.contains("`alpha` up to date @ bbbbbbb2"), "{text}");
    }

    #[test]
    fn a_package_whose_path_is_gone_is_refused_and_its_source_is_untouched() {
        let c = installed();
        c.move_source();
        std::fs::remove_dir_all(
            c.fetcher
                .fixture
                .join(".localpilot")
                .join("skills")
                .join("beta"),
        )
        .unwrap();
        let before = (c.body("alpha"), c.body("beta"), c.ledger());
        let (r, text) = c.update(None);
        assert!(r.is_err());
        assert!(
            text.contains("`beta`: its path") && text.contains("reinstall it explicitly"),
            "{text}"
        );
        assert_eq!((c.body("alpha"), c.body("beta"), c.ledger()), before);
    }

    #[test]
    fn unmanaged_names_and_unattended_runs_are_refused() {
        let c = installed();
        let mut out = Vec::new();
        let err = c
            .manager()
            .update(
                Scope::Project,
                UpdateTarget::Named("nope".into()),
                Approval::AssumeYes,
                &mut out,
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("was not installed by LocalPilot"),
            "{err}"
        );
        let err = c
            .manager()
            .update(
                Scope::Project,
                UpdateTarget::All,
                Approval::NonInteractive,
                &mut out,
            )
            .unwrap_err();
        assert!(err.to_string().contains("approval required"), "{err}");
    }

    fn assert_at_a(c: &Ctx, ledger: &str) {
        assert!(c.body("alpha").unwrap().contains("alpha A"));
        assert!(c.body("beta").unwrap().contains("beta A"));
        assert_eq!(c.ledger(), ledger);
        assert!(c.leftovers().is_empty(), "{:?}", c.leftovers());
    }

    #[test]
    fn a_failed_ledger_save_or_swap_rolls_back_to_the_old_copies() {
        for fault in [Fault::LedgerSave, Fault::SwapFails(1), Fault::SwapFails(0)] {
            let c = installed();
            let ledger = c.ledger();
            c.move_source();
            let (r, _) = c.update(Some(fault));
            assert!(r.is_err(), "{fault:?}");
            assert_at_a(&c, &ledger);
        }
    }

    #[test]
    fn a_crash_mid_swap_is_rolled_back_by_the_next_mutation_and_never_shows_two_copies() {
        let c = installed();
        let ledger = c.ledger();
        c.move_source();
        let (r, _) = c.update(Some(Fault::StopAfterSwap(1)));
        assert!(r.is_err());
        // Half swapped: alpha is new, beta old; neither aside nor staging is
        // a candidate.
        let set = c.effective();
        assert!(set
            .by_name("alpha")
            .unwrap()
            .instructions
            .contains("alpha B"));
        assert!(set.by_name("beta").unwrap().instructions.contains("beta A"));
        assert!(set.shadowed().is_empty(), "{:?}", set.shadowed());
        assert!(c
            .manager()
            .recovery_pending(ReadScope::Effective)
            .contains(&Scope::Project));
        let text = c.recover();
        assert!(
            text.contains("rolled back an interrupted skills update"),
            "{text}"
        );
        assert_at_a(&c, &ledger);
        assert!(!c.recover().contains("recovered"));
        assert_at_a(&c, &ledger);
    }

    #[test]
    fn a_crash_after_commit_is_finished_by_the_next_mutation() {
        let c = installed();
        c.move_source();
        let (r, _) = c.update(Some(Fault::StopAfterCommit));
        assert!(r.is_err());
        let set = c.effective();
        assert!(set
            .by_name("alpha")
            .unwrap()
            .instructions
            .contains("alpha B"));
        assert!(set.by_name("beta").unwrap().instructions.contains("beta B"));
        assert!(set.shadowed().is_empty(), "{:?}", set.shadowed());
        assert!(!c.leftovers().is_empty(), "the asides are still there");
        let text = c.recover();
        assert!(
            text.contains("finished an interrupted skills update"),
            "{text}"
        );
        assert!(c.leftovers().is_empty(), "{:?}", c.leftovers());
        assert!(c.ledger().contains("bbbbbbb2"));
        c.recover();
        assert!(c.body("beta").unwrap().contains("beta B"));
    }

    #[test]
    fn a_missing_managed_skill_is_reported_restored_by_update_and_left_absent_by_a_rollback() {
        let c = installed();
        std::fs::remove_dir_all(c.base().join("skills").join("alpha")).unwrap();
        let managed = c.manager().managed(ReadScope::Effective).unwrap();
        assert!(managed.iter().any(|m| m.name == "alpha" && m.missing));
        let ledger = c.ledger();
        c.move_source();
        let (r, _) = c.update(Some(Fault::StopAfterSwap(2)));
        assert!(r.is_err());
        c.recover();
        c.recover();
        assert!(
            c.body("alpha").is_none(),
            "a rollback leaves the missing skill missing"
        );
        assert!(c.body("beta").unwrap().contains("beta A"));
        assert_eq!(c.ledger(), ledger);
        let (r, text) = c.update(None);
        r.unwrap();
        assert!(text.contains("reinstalled `alpha`"), "{text}");
        assert!(c.body("alpha").unwrap().contains("alpha B"));
    }

    fn refresh(c: &Ctx, fault: Option<Fault>) -> Result<(), SkillError> {
        let mut m = c.manager();
        if let Some(f) = fault {
            m = m.with_fault(f);
        }
        m.repo_refresh(Scope::Project, None, Approval::AssumeYes, &mut Vec::new())
    }

    #[test]
    fn a_failed_or_interrupted_cache_swap_keeps_cache_and_registry_together() {
        // A caught rename failure: the old cache and commit stay.
        let c = installed();
        c.move_source();
        assert!(refresh(&c, Some(Fault::CacheSwap)).is_err());
        assert!(c.cached_body("alpha").contains("alpha A"));
        assert_eq!(c.registry_commit(), "aaaaaaa1");
        // Stopped with the old cache aside: recovery puts it back.
        assert!(refresh(&c, Some(Fault::StopAfterCacheAside)).is_err());
        assert!(c.recover().contains("rolled back an interrupted refresh"));
        assert!(c.cached_body("alpha").contains("alpha A"));
        assert_eq!(c.registry_commit(), "aaaaaaa1");
        c.recover();
        // Stopped after the new cache is in place, before the registry moved:
        // recovery records the new commit, so the two agree.
        assert!(refresh(&c, Some(Fault::StopAfterCacheFinal)).is_err());
        assert_eq!(c.registry_commit(), "aaaaaaa1");
        assert!(c.recover().contains("finished an interrupted refresh"));
        assert!(c.cached_body("alpha").contains("alpha B"));
        assert_eq!(c.registry_commit(), "bbbbbbb2");
        let repos = c.base().join("skill-repos");
        let stray: Vec<String> = std::fs::read_dir(&repos)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with('.'))
            .collect();
        assert!(stray.is_empty(), "{stray:?}");
    }

    #[test]
    fn an_orphaned_old_cache_is_removed_by_a_later_mutation() {
        let c = installed();
        let id = source_id(&normalize_url(URL).unwrap());
        let orphan = c.base().join("skill-repos").join(format!(".old-{id}"));
        std::fs::create_dir_all(&orphan).unwrap();
        c.recover();
        assert!(!orphan.exists());
    }

    #[test]
    fn a_project_install_from_a_global_source_goes_stale_and_updates_from_that_cache() {
        // Bug it prevents: a project install drawn from a global source (as
        // install allows) reported as unregistered, and never stale.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = tmp.path().join("project");
        let fixture = tmp.path().join("fixture");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        write_skill(&fixture, "alpha", "alpha A");
        let c = Ctx {
            _tmp: tmp,
            home,
            project,
            fetcher: MovingFetcher {
                fixture,
                commit: Mutex::new("aaaaaaa1".into()),
            },
        };
        let mut out = Vec::new();
        c.manager()
            .repo_add(Scope::Global, URL, Approval::AssumeYes, &mut out)
            .unwrap();
        c.manager()
            .install(
                Scope::Project,
                InstallSpec::Named {
                    name: "alpha".into(),
                    repo: None,
                },
                Approval::AssumeYes,
                &mut out,
            )
            .unwrap();
        write_skill(&c.fetcher.fixture, "alpha", "alpha B");
        *c.fetcher.commit.lock().unwrap() = "bbbbbbb2".into();
        c.manager()
            .repo_refresh(Scope::Global, None, Approval::AssumeYes, &mut out)
            .unwrap();

        let managed = c.manager().managed(ReadScope::Effective).unwrap();
        let alpha = managed.iter().find(|m| m.scope == Scope::Project).unwrap();
        assert_eq!(alpha.stale(), Some(("aaaaaaa1", "bbbbbbb2")));

        let (r, text) = c.update(None);
        r.unwrap();
        assert!(
            text.contains("is a global source; using its cache @ bbbbbbb2"),
            "{text}"
        );
        assert!(
            text.contains("updated `alpha`: aaaaaaa1 -> bbbbbbb2"),
            "{text}"
        );
        assert!(c.body("alpha").unwrap().contains("alpha B"));
        assert!(c
            .manager()
            .managed(ReadScope::Effective)
            .unwrap()
            .iter()
            .all(|m| m.stale().is_none()));
    }

    #[test]
    fn a_swap_stopped_or_failed_before_any_rename_keeps_the_old_cache_and_commit() {
        // Bug it prevents: a marker left before the old cache moved being read
        // as a finished swap, so the registry claims B over a cache still at A.
        let c = installed();
        c.move_source();
        assert!(refresh(&c, Some(Fault::StopAfterMarker)).is_err());
        assert!(c.recover().contains("rolled back an interrupted refresh"));
        assert!(c.cached_body("alpha").contains("alpha A"));
        assert_eq!(c.registry_commit(), "aaaaaaa1");
        assert!(refresh(&c, Some(Fault::CacheAsideFails)).is_err());
        assert!(
            !c.recover().contains("recovered"),
            "a caught failure leaves nothing to recover"
        );
        assert!(c.cached_body("alpha").contains("alpha A"));
        assert_eq!(c.registry_commit(), "aaaaaaa1");
        let repos = c.base().join("skill-repos");
        let stray: Vec<String> = std::fs::read_dir(&repos)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with('.'))
            .collect();
        assert!(stray.is_empty(), "{stray:?}");
    }

    fn hold(base: &Path) -> fd_lock::RwLock<std::fs::File> {
        std::fs::create_dir_all(base).unwrap();
        fd_lock::RwLock::new(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(base.join("skills.lock"))
                .unwrap(),
        )
    }

    /// A live update in another holder: its journal must survive a second
    /// manager, which waits, then refuses, and recovers nothing.
    fn assert_refused_while_held(c: &Ctx) {
        let journal = update::update_dir(&c.base()).join("journal.toml");
        std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
        std::fs::write(&journal, "phase = \"swapping\"\n").unwrap();
        let mut out = Vec::new();
        let err = c
            .manager()
            .with_lock_wait(Duration::from_millis(300))
            .update(
                Scope::Project,
                UpdateTarget::All,
                Approval::AssumeYes,
                &mut out,
            )
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("another localpilot skills command is changing the project scope"),
            "{err}"
        );
        assert!(
            journal.exists(),
            "a live update's journal was treated as interrupted"
        );
        std::fs::remove_file(&journal).unwrap();
    }

    #[test]
    fn a_change_waits_for_another_holder_of_the_scope_and_then_refuses() {
        let c = installed();
        let mut lock = hold(&c.base());
        let held = lock.try_write().unwrap();
        assert_refused_while_held(&c);
        drop(held);
        let (r, _) = c.update(None);
        r.unwrap();
    }

    const HOLD_ENV: &str = "LOCALPILOT_SKILLS_TEST_HOLD_SCOPE";

    /// Not a test on its own: the child process of the cross-process test,
    /// which holds the scope lock until told to let go.
    #[test]
    fn hold_the_scope_lock_for_the_cross_process_test() {
        let Ok(base) = std::env::var(HOLD_ENV) else {
            return;
        };
        let base = PathBuf::from(base);
        let mut lock = hold(&base);
        let _held = lock.try_write().unwrap();
        std::fs::write(base.join("held"), "").unwrap();
        let end = Instant::now() + Duration::from_secs(60);
        while !base.join("release").exists() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_change_waits_for_another_process_holding_the_scope() {
        let c = installed();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "manager::update_tests::hold_the_scope_lock_for_the_cross_process_test",
                "--nocapture",
            ])
            .env(HOLD_ENV, c.base())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let end = Instant::now() + Duration::from_secs(30);
        while !c.base().join("held").exists() {
            assert!(Instant::now() < end, "the child never took the lock");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_refused_while_held(&c);
        std::fs::write(c.base().join("release"), "").unwrap();
        assert!(child.wait().unwrap().success());
        std::fs::remove_file(c.base().join("held")).unwrap();
        std::fs::remove_file(c.base().join("release")).unwrap();
        let (r, _) = c.update(None);
        r.unwrap();
    }
}
