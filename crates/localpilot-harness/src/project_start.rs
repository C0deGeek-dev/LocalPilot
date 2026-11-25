//! Empty-project preparation shared by ordinary and harness agent turns.
//! Inspection is read-only; the caller must authorize `initialize` separately.

use std::{path::Path, process::Command, time::Duration};

use crate::HarnessError;

/// An author/committer field Git cannot resolve for the baseline commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdentityField {
    Name,
    Email,
}

impl IdentityField {
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Name => "user.name",
            Self::Email => "user.email",
        }
    }
}

/// Where a user-supplied identity is stored. Repository-local is the default;
/// global is written only when the user chose it explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdentityScope {
    Repository,
    Global,
}

/// Identity values the user supplied for the fields Git was missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitIdentity {
    pub(crate) name: Option<String>,
    pub(crate) email: Option<String>,
    pub(crate) scope: IdentityScope,
}

impl GitIdentity {
    fn value(&self, field: IdentityField) -> Option<&str> {
        match field {
            IdentityField::Name => self.name.as_deref(),
            IdentityField::Email => self.email.as_deref(),
        }
    }
}

/// Command-local stand-ins used only to learn which field restores both
/// identities; they are never written anywhere.
const PROBE_NAME: &str = "user.name=LocalPilot identity probe";
const PROBE_EMAIL: &str = "user.email=identity-probe@localpilot.invalid";

/// The recovery shown when Git has no usable identity and none was supplied.
pub(crate) fn identity_recovery(root: &Path, missing: &[IdentityField]) -> String {
    let fields = missing
        .iter()
        .map(|field| field.key())
        .collect::<Vec<_>>()
        .join(" and ");
    let commands = missing
        .iter()
        .map(|field| match field {
            IdentityField::Name => "git config user.name \"Your Name\"",
            IdentityField::Email => "git config user.email \"you@example.com\"",
        })
        .collect::<Vec<_>>()
        .join(" and ");
    format!(
        "Git has no author identity for the empty baseline commit (missing {fields}). In {}, run {commands} with your real details; this stores them only in this repository's .git/config. Then retry. Add --global only if you want the identity for every repository. Do not invent placeholder values or change the global Git configuration without the user's explicit choice.",
        root.display()
    )
}

pub(crate) struct ProjectGit {
    program: std::ffi::OsString,
    // Command-local configuration also gives native tests an isolated Git home.
    environment: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    #[cfg(test)]
    isolated: bool,
}

impl Default for ProjectGit {
    fn default() -> Self {
        Self {
            program: "git".into(),
            environment: Vec::new(),
            #[cfg(test)]
            isolated: false,
        }
    }
}

impl ProjectGit {
    #[cfg(test)]
    #[allow(clippy::unwrap_used)] // Disposable test configuration; never production.
    pub(crate) fn isolated(home: &Path, identity: bool) -> Self {
        let config = home.join("gitconfig");
        std::fs::write(&config, if identity {
            "[user]\n name = Fixture\n email = fixture@example.invalid\n[commit]\n gpgSign = false\n"
        } else { "[user]\n useConfigOnly = true\n" }).unwrap();
        Self {
            isolated: true,
            environment: vec![
                ("HOME".into(), home.as_os_str().into()),
                ("USERPROFILE".into(), home.as_os_str().into()),
                ("GIT_CONFIG_GLOBAL".into(), config.into_os_string()),
                ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ],
            ..Self::default()
        }
    }
    fn run(&self, root: &Path, args: &[&str]) -> Result<String, HarnessError> {
        self.run_labeled(root, args, &args.join(" "))
    }

    /// `label` names the command in errors; callers passing user-supplied
    /// values give one without them, so failures never echo those values.
    fn run_labeled(&self, root: &Path, args: &[&str], label: &str) -> Result<String, HarnessError> {
        let mut command = Command::new(&self.program);
        #[cfg(test)]
        if self.isolated {
            command.env_clear();
        }
        for variable in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        ] {
            command.env_remove(variable);
        }
        command
            .args(["--literal-pathspecs", "-c", "core.fsmonitor=false"])
            .args(args)
            .current_dir(root)
            .envs(self.environment.iter().map(|(key, value)| (key, value)));
        crate::resume::bounded_output(
            command,
            Duration::from_secs(30),
            &format!("project Git setup ({label})"),
        )
        .map_err(|error| match error {
            HarnessError::Provider(message) => HarnessError::ProjectSetup(message),
            other => other,
        })
    }

    /// Only a root with no project material can be adopted automatically.
    /// Known LocalPilot/LocalMind local state is allowed, but symlinks are not.
    fn effectively_empty(root: &Path) -> Result<bool, HarnessError> {
        let io = |source| HarnessError::Io {
            path: root.display().to_string(),
            source,
        };
        for entry in std::fs::read_dir(root).map_err(io)? {
            let entry = entry.map_err(io)?;
            let name = entry.file_name();
            let kind = entry.file_type().map_err(io)?;
            let allowed = match name.to_str() {
                Some(".git" | ".localpilot" | ".localmind") => kind.is_dir(),
                Some(".localpilot.toml" | ".localmind.toml") => kind.is_file(),
                _ => false,
            };
            if !allowed || kind.is_symlink() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn identity_resolves(&self, root: &Path, overrides: &[&str]) -> Result<(), HarnessError> {
        for variable in ["GIT_AUTHOR_IDENT", "GIT_COMMITTER_IDENT"] {
            let mut args = Vec::with_capacity(overrides.len() * 2 + 2);
            for value in overrides {
                args.extend(["-c", value]);
            }
            args.extend(["var", variable]);
            self.run(root, &args)?;
        }
        Ok(())
    }

    /// The fields Git cannot resolve for both the author and the committer,
    /// using the same strict check a commit uses. Command-local probes find
    /// which field restores both, so environment and `author.*`/`committer.*`
    /// overrides that already provide a value are respected.
    pub(crate) fn missing_identity(&self, root: &Path) -> Result<Vec<IdentityField>, HarnessError> {
        if self.identity_resolves(root, &[]).is_ok() {
            return Ok(Vec::new());
        }
        if self.identity_resolves(root, &[PROBE_EMAIL]).is_ok() {
            return Ok(vec![IdentityField::Email]);
        }
        if self.identity_resolves(root, &[PROBE_NAME]).is_ok() {
            return Ok(vec![IdentityField::Name]);
        }
        // Supplying both must work; otherwise Git's own reason is the real one.
        self.identity_resolves(root, &[PROBE_NAME, PROBE_EMAIL])?;
        Ok(vec![IdentityField::Name, IdentityField::Email])
    }

    pub(crate) fn needs_baseline(&self, root: &Path) -> Result<bool, HarnessError> {
        if !Self::effectively_empty(root)? {
            return Ok(false);
        }
        // Git's own discovery is authoritative, including parent repositories.
        if let Ok(owner) = self.run(root, &["rev-parse", "--show-toplevel"]) {
            let owner = std::fs::canonicalize(owner.trim()).map_err(|source| HarnessError::Io {
                path: owner.trim().to_string(),
                source,
            })?;
            let root = std::fs::canonicalize(root).map_err(|source| HarnessError::Io {
                path: root.display().to_string(),
                source,
            })?;
            if owner != root || self.run(&root, &["rev-parse", "--verify", "HEAD"]).is_ok() {
                return Ok(false);
            }
            // An unborn branch must really be unborn, rather than a broken HEAD.
            let branch = self.run(&root, &["symbolic-ref", "HEAD"])?;
            if !self
                .run(&root, &["for-each-ref", branch.trim()])?
                .trim()
                .is_empty()
            {
                return Err(HarnessError::ProjectSetup(
                    "HEAD cannot be resolved; repair this repository before starting writes".into(),
                ));
            }
            if !self.run(&root, &["ls-files", "--stage"])?.trim().is_empty() {
                return Err(HarnessError::ProjectSetup("the unborn repository has staged files; commit or unstage them yourself before requesting an empty baseline".into()));
            }
        } else if root
            .ancestors()
            .any(|ancestor| ancestor.join(".git").exists())
        {
            // Keep the real Git error for corrupt repositories or missing Git.
            self.run(root, &["rev-parse", "--show-toplevel"])?;
        } else {
            // A failed discovery alone cannot distinguish an ordinary empty
            // folder from an unavailable executable. Do not hide that failure
            // behind a setup consent request or pretend initialization ran.
            self.run(root, &["--version"])?;
        }
        Ok(true)
    }

    /// Called only after permission. Recheck after approval, create the
    /// repository and its local exclusions, then report the identity fields a
    /// commit would miss. Git's identity is probed only after `git init`, so
    /// `includeIf.gitdir` configuration for the new repository counts.
    /// `None` when no baseline is needed.
    pub(crate) fn prepare(&self, root: &Path) -> Result<Option<Vec<IdentityField>>, HarnessError> {
        if !self.needs_baseline(root)? {
            return Ok(None);
        }
        if !root.join(".git").exists() {
            self.run(root, &["init", "-q"])?;
        }
        let exclude = self.run(root, &["rev-parse", "--git-path", "info/exclude"])?;
        let exclude = root.join(exclude.trim());
        let io = |source| HarnessError::Io {
            path: exclude.display().to_string(),
            source,
        };
        if let Some(parent) = exclude.parent() {
            if std::fs::symlink_metadata(parent)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                return Err(HarnessError::ProjectSetup("Git's info directory is a symlink; configure local exclusions yourself before retrying".into()));
            }
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        if std::fs::symlink_metadata(&exclude)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(HarnessError::ProjectSetup("Git's local exclude file is a symlink; configure local exclusions yourself before retrying".into()));
        }
        let mut text = match std::fs::read_to_string(&exclude) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(io(error)),
        };
        for entry in crate::resume::HARNESS_LOCAL_PATHS {
            if !text.lines().any(|line| line.trim() == entry) {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(entry);
                text.push('\n');
            }
        }
        std::fs::write(&exclude, text).map_err(io)?;
        Ok(Some(self.missing_identity(root)?))
    }

    /// Git follows a symlinked config file when it writes, so a
    /// repository-only identity must not land in a linked user-level file.
    fn guard_local_config(&self, root: &Path) -> Result<(), HarnessError> {
        let config = self.run(root, &["rev-parse", "--git-path", "config"])?;
        let config = root.join(config.trim());
        let refuse = || {
            HarnessError::ProjectSetup(format!(
                "this repository's Git config ({}) is linked or redirected outside the project's .git directory, so a repository-only identity cannot be saved safely; repair the repository link, or configure the identity yourself, before retrying",
                config.display()
            ))
        };
        // An unborn repository may have no config yet; Git creates it in the
        // verified .git directory. Links and other metadata errors are refused.
        let config_ok = match std::fs::symlink_metadata(&config) {
            Ok(metadata) => metadata.file_type().is_file(),
            Err(error) => error.kind() == std::io::ErrorKind::NotFound,
        };
        let git_dir = root.join(".git");
        let git_dir_ok =
            std::fs::symlink_metadata(&git_dir).is_ok_and(|metadata| metadata.file_type().is_dir());
        if !config_ok || !git_dir_ok {
            return Err(refuse());
        }
        // A commondir or gitdir indirection must resolve back to this .git.
        let canonical = |path: &Path| std::fs::canonicalize(path).map_err(|_| refuse());
        let parent = config.parent().ok_or_else(refuse)?;
        if canonical(parent)? != canonical(&git_dir)? {
            return Err(refuse());
        }
        Ok(())
    }

    /// Called only after permission. Prepares the repository, then commits the
    /// empty baseline. `identity` holds values the user supplied; only fields
    /// still missing at this point are written, in the chosen scope, so a
    /// configured identity is never replaced.
    pub(crate) fn initialize(
        &self,
        root: &Path,
        identity: Option<&GitIdentity>,
    ) -> Result<bool, HarnessError> {
        let Some(mut missing) = self.prepare(root)? else {
            return Ok(false);
        };
        // Check the identity before committing, so a missing one fails with a
        // repository-local recovery rather than Git's global-config advice.
        // Completed setup above is kept either way.
        if let Some(identity) = identity.filter(|_| !missing.is_empty()) {
            let scope = match identity.scope {
                IdentityScope::Repository => {
                    self.guard_local_config(root)?;
                    "--local"
                }
                IdentityScope::Global => "--global",
            };
            for field in &missing {
                if let Some(value) = identity.value(*field) {
                    let label = format!("config {scope} {}", field.key());
                    self.run_labeled(root, &["config", scope, field.key(), value], &label)?;
                }
            }
            missing = self.missing_identity(root)?;
        }
        if !missing.is_empty() {
            return Err(HarnessError::ProjectSetup(identity_recovery(
                root, &missing,
            )));
        }
        // --only with --allow-empty records no paths, even if another process
        // stages material after inspection. Never add, reset or stash anything.
        self.run(
            root,
            &[
                "commit",
                "--only",
                "--allow-empty",
                "-m",
                "Initialize empty project baseline",
            ],
        )?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    struct Fixture {
        _dir: tempfile::TempDir,
        root: std::path::PathBuf,
        git: ProjectGit,
    }

    impl Fixture {
        fn new(identity: bool) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("project");
            std::fs::create_dir(&root).unwrap();
            let git = ProjectGit::isolated(dir.path(), identity);
            Self {
                _dir: dir,
                root,
                git,
            }
        }
    }

    #[test]
    fn empty_and_runtime_only_projects_get_a_zero_file_baseline() {
        for runtime_state in [false, true] {
            let f = Fixture::new(true);
            if runtime_state {
                std::fs::create_dir(f.root.join(".localpilot")).unwrap();
                std::fs::write(f.root.join(".localpilot/state"), "local state").unwrap();
                std::fs::write(f.root.join(".localpilot.toml"), "# local config").unwrap();
            }
            assert!(f.git.needs_baseline(&f.root).unwrap());
            assert!(f.git.initialize(&f.root, None).unwrap());
            let head = f.git.run(&f.root, &["rev-parse", "HEAD"]).unwrap();
            assert!(f
                .git
                .run(&f.root, &["ls-tree", "-r", "HEAD"])
                .unwrap()
                .is_empty());
            assert!(f.git.run(&f.root, &["ls-files"]).unwrap().is_empty());
            assert!(f
                .git
                .run(&f.root, &["status", "--porcelain"])
                .unwrap()
                .is_empty());
            assert!(!f.git.initialize(&f.root, None).unwrap());
            assert_eq!(head, f.git.run(&f.root, &["rev-parse", "HEAD"]).unwrap());
            assert!(crate::resume::work_diff_baseline(&f.root).is_ok());
        }
    }

    #[test]
    fn unborn_repository_can_receive_an_empty_baseline() {
        let f = Fixture::new(true);
        f.git.run(&f.root, &["init", "-q"]).unwrap();
        assert!(f.git.initialize(&f.root, None).unwrap());
        assert!(f
            .git
            .run(&f.root, &["ls-tree", "-r", "HEAD"])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn parent_repository_is_never_shadowed_even_without_head() {
        for head in [false, true] {
            let f = Fixture::new(true);
            f.git.run(&f.root, &["init", "-q"]).unwrap();
            if head {
                f.git.initialize(&f.root, None).unwrap();
            }
            let child = f.root.join("child");
            std::fs::create_dir(&child).unwrap();
            assert!(!f.git.initialize(&child, None).unwrap());
            assert!(!child.join(".git").exists());
        }
    }

    #[test]
    fn nonempty_project_and_staged_index_are_preserved() {
        let f = Fixture::new(true);
        std::fs::write(f.root.join("user.txt"), "keep").unwrap();
        assert!(!f.git.initialize(&f.root, None).unwrap());
        assert!(!f.root.join(".git").exists());
        f.git.run(&f.root, &["init", "-q"]).unwrap();
        f.git.run(&f.root, &["add", "user.txt"]).unwrap();
        let index = std::fs::read(f.root.join(".git/index")).unwrap();
        // Removing the worktree copy must not make the staged file agent-owned.
        std::fs::remove_file(f.root.join("user.txt")).unwrap();
        assert!(f
            .git
            .initialize(&f.root, None)
            .unwrap_err()
            .to_string()
            .contains("staged files"));
        assert_eq!(index, std::fs::read(f.root.join(".git/index")).unwrap());
        assert!(f
            .git
            .run(&f.root, &["rev-parse", "--verify", "HEAD"])
            .is_err());
    }

    fn config(f: &Fixture, scope: &str, key: &str) -> Option<String> {
        f.git
            .run(&f.root, &["config", scope, "--get", key])
            .ok()
            .map(|value| value.trim().to_string())
    }

    #[test]
    fn missing_identity_reports_only_the_fields_git_cannot_resolve() {
        assert!(Fixture::new(true)
            .git
            .missing_identity(&Fixture::new(true).root)
            .unwrap()
            .is_empty());
        let f = Fixture::new(false);
        assert_eq!(
            f.git.missing_identity(&f.root).unwrap(),
            vec![IdentityField::Name, IdentityField::Email]
        );
        // An effective name from the environment is kept; only email is asked.
        let mut f = Fixture::new(false);
        f.git.environment.extend([
            ("GIT_AUTHOR_NAME".into(), "Existing Author".into()),
            ("GIT_COMMITTER_NAME".into(), "Existing Committer".into()),
        ]);
        assert_eq!(
            f.git.missing_identity(&f.root).unwrap(),
            vec![IdentityField::Email]
        );
    }

    #[test]
    fn supplied_identity_is_repository_local_by_default_and_keeps_effective_names() {
        let mut f = Fixture::new(false);
        f.git.environment.extend([
            ("GIT_AUTHOR_NAME".into(), "Existing Author".into()),
            ("GIT_COMMITTER_NAME".into(), "Existing Committer".into()),
        ]);
        let global = std::fs::read(f._dir.path().join("gitconfig")).unwrap();
        let identity = GitIdentity {
            name: Some("Ignored Name".into()),
            email: Some("person@example.invalid".into()),
            scope: IdentityScope::Repository,
        };
        assert!(f.git.initialize(&f.root, Some(&identity)).unwrap());
        assert_eq!(
            global,
            std::fs::read(f._dir.path().join("gitconfig")).unwrap()
        );
        assert_eq!(
            config(&f, "--local", "user.email").as_deref(),
            Some("person@example.invalid")
        );
        assert_eq!(config(&f, "--local", "user.name"), None);
        let author = f
            .git
            .run(&f.root, &["log", "-1", "--format=%an <%ae>|%cn"])
            .unwrap();
        assert_eq!(
            author.trim(),
            "Existing Author <person@example.invalid>|Existing Committer"
        );
    }

    #[cfg(unix)]
    #[test]
    fn repository_identity_is_not_written_through_a_linked_config() {
        let f = Fixture::new(false);
        f.git.run(&f.root, &["init", "-q"]).unwrap();
        let outside = f._dir.path().join("linked-config");
        std::fs::rename(f.root.join(".git/config"), &outside).unwrap();
        std::os::unix::fs::symlink(&outside, f.root.join(".git/config")).unwrap();
        let before = std::fs::read(&outside).unwrap();
        let identity = GitIdentity {
            name: Some("Person".into()),
            email: Some("person@example.invalid".into()),
            scope: IdentityScope::Repository,
        };
        let error = f.git.initialize(&f.root, Some(&identity)).unwrap_err();
        assert!(error.to_string().contains("linked"), "{error}");
        assert_eq!(before, std::fs::read(&outside).unwrap());
    }

    #[test]
    fn identity_scoped_to_the_new_repository_is_seen_after_init() {
        let f = Fixture::new(false);
        let included = f._dir.path().join("project-identity");
        std::fs::write(
            &included,
            "[user]\n name = Scoped Person\n email = scoped@example.invalid\n",
        )
        .unwrap();
        let global = f._dir.path().join("gitconfig");
        let mut text = std::fs::read_to_string(&global).unwrap();
        text.push_str(&format!(
            "[includeIf \"gitdir/i:**/project/\"]\n path = {}\n[commit]\n gpgSign = false\n",
            included.display().to_string().replace('\\', "/")
        ));
        std::fs::write(&global, text).unwrap();
        // Before init there is no gitdir for the condition to match.
        assert!(!f.git.missing_identity(&f.root).unwrap().is_empty());
        assert_eq!(f.git.prepare(&f.root).unwrap(), Some(Vec::new()));
        assert!(f.git.initialize(&f.root, None).unwrap());
        assert_eq!(config(&f, "--local", "user.name"), None);
    }

    #[test]
    fn a_failed_identity_write_does_not_echo_the_supplied_values() {
        let f = Fixture::new(false);
        f.git.run(&f.root, &["init", "-q"]).unwrap();
        std::fs::write(f.root.join(".git/config.lock"), "").unwrap();
        let identity = GitIdentity {
            name: Some("Private Person".into()),
            email: Some("private@example.invalid".into()),
            scope: IdentityScope::Repository,
        };
        let error = f
            .git
            .initialize(&f.root, Some(&identity))
            .unwrap_err()
            .to_string();
        assert!(error.contains("config --local user.name"), "{error}");
        assert!(
            !error.contains("Private Person") && !error.contains("private@example"),
            "{error}"
        );
        assert!(f
            .git
            .run(&f.root, &["rev-parse", "--verify", "HEAD"])
            .is_err());
    }

    #[test]
    fn an_unborn_repository_without_a_config_file_gets_a_local_identity() {
        let f = Fixture::new(false);
        f.git.run(&f.root, &["init", "-q"]).unwrap();
        std::fs::remove_file(f.root.join(".git/config")).unwrap();
        let identity = GitIdentity {
            name: Some("Person".into()),
            email: Some("person@example.invalid".into()),
            scope: IdentityScope::Repository,
        };
        assert!(f.git.initialize(&f.root, Some(&identity)).unwrap());
        assert_eq!(
            config(&f, "--local", "user.name").as_deref(),
            Some("Person")
        );
    }

    #[test]
    fn a_redirected_common_directory_is_refused_for_a_repository_identity() {
        let f = Fixture::new(false);
        f.git.run(&f.root, &["init", "-q"]).unwrap();
        let elsewhere = f._dir.path().join("elsewhere");
        let status = std::process::Command::new("git")
            .args(["init", "-q", "--bare"])
            .arg(&elsewhere)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", f._dir.path().join("gitconfig"))
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::write(
            f.root.join(".git/commondir"),
            elsewhere.display().to_string(),
        )
        .unwrap();
        let before = std::fs::read(elsewhere.join("config")).unwrap();
        let identity = GitIdentity {
            name: Some("Person".into()),
            email: Some("person@example.invalid".into()),
            scope: IdentityScope::Repository,
        };
        let error = f
            .git
            .initialize(&f.root, Some(&identity))
            .unwrap_err()
            .to_string();
        assert!(error.contains("redirected"), "{error}");
        assert_eq!(before, std::fs::read(elsewhere.join("config")).unwrap());
    }

    #[test]
    fn global_identity_is_written_only_when_chosen() {
        let f = Fixture::new(false);
        let identity = GitIdentity {
            name: Some("Chosen Person".into()),
            email: Some("chosen@example.invalid".into()),
            scope: IdentityScope::Global,
        };
        assert!(f.git.initialize(&f.root, Some(&identity)).unwrap());
        assert_eq!(config(&f, "--local", "user.name"), None);
        assert_eq!(
            config(&f, "--global", "user.name").as_deref(),
            Some("Chosen Person")
        );
        assert_eq!(
            config(&f, "--global", "user.email").as_deref(),
            Some("chosen@example.invalid")
        );
    }

    #[test]
    fn identity_and_executable_failures_preserve_partial_setup_and_real_reason() {
        let mut f = Fixture::new(false);
        let global = std::fs::read(f._dir.path().join("gitconfig")).unwrap();
        let error = f.git.initialize(&f.root, None).unwrap_err().to_string();
        assert!(
            error.contains("missing user.name and user.email")
                && error.contains("this repository's .git/config")
                && !error.contains("git config --global"),
            "{error}"
        );
        assert_eq!(
            global,
            std::fs::read(f._dir.path().join("gitconfig")).unwrap()
        );
        assert!(f
            .git
            .run(&f.root, &["rev-parse", "--verify", "HEAD"])
            .is_err());
        assert!(f.root.join(".git").is_dir());
        assert!(f.git.run(&f.root, &["ls-files"]).unwrap().is_empty());
        f.git.program = f._dir.path().join("missing-git.exe").into_os_string();
        assert!(f.git.initialize(&f.root, None).is_err());
        assert!(f.root.join(".git").is_dir());
        let empty = f._dir.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        assert!(f.git.needs_baseline(&empty).is_err());
        assert!(!empty.join(".git").exists());
    }
}
