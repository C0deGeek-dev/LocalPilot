//! Empty-project preparation shared by ordinary and harness agent turns.
//! Inspection is read-only; the caller must authorize `initialize` separately.

use std::{path::Path, process::Command, time::Duration};

use crate::HarnessError;

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
            &format!("project Git setup ({})", args.join(" ")),
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

    /// Called only after permission. Recheck after approval before changing disk.
    pub(crate) fn initialize(&self, root: &Path) -> Result<bool, HarnessError> {
        if !self.needs_baseline(root)? {
            return Ok(false);
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
            assert!(f.git.initialize(&f.root).unwrap());
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
            assert!(!f.git.initialize(&f.root).unwrap());
            assert_eq!(head, f.git.run(&f.root, &["rev-parse", "HEAD"]).unwrap());
            assert!(crate::resume::work_diff_baseline(&f.root).is_ok());
        }
    }

    #[test]
    fn unborn_repository_can_receive_an_empty_baseline() {
        let f = Fixture::new(true);
        f.git.run(&f.root, &["init", "-q"]).unwrap();
        assert!(f.git.initialize(&f.root).unwrap());
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
                f.git.initialize(&f.root).unwrap();
            }
            let child = f.root.join("child");
            std::fs::create_dir(&child).unwrap();
            assert!(!f.git.initialize(&child).unwrap());
            assert!(!child.join(".git").exists());
        }
    }

    #[test]
    fn nonempty_project_and_staged_index_are_preserved() {
        let f = Fixture::new(true);
        std::fs::write(f.root.join("user.txt"), "keep").unwrap();
        assert!(!f.git.initialize(&f.root).unwrap());
        assert!(!f.root.join(".git").exists());
        f.git.run(&f.root, &["init", "-q"]).unwrap();
        f.git.run(&f.root, &["add", "user.txt"]).unwrap();
        let index = std::fs::read(f.root.join(".git/index")).unwrap();
        // Removing the worktree copy must not make the staged file agent-owned.
        std::fs::remove_file(f.root.join("user.txt")).unwrap();
        assert!(f
            .git
            .initialize(&f.root)
            .unwrap_err()
            .to_string()
            .contains("staged files"));
        assert_eq!(index, std::fs::read(f.root.join(".git/index")).unwrap());
        assert!(f
            .git
            .run(&f.root, &["rev-parse", "--verify", "HEAD"])
            .is_err());
    }

    #[test]
    fn identity_and_executable_failures_preserve_partial_setup_and_real_reason() {
        let mut f = Fixture::new(false);
        let error = f.git.initialize(&f.root).unwrap_err().to_string();
        assert!(
            error.contains("identity")
                || error.contains("user.name")
                || error.contains("user.email"),
            "{error}"
        );
        assert!(f.root.join(".git").is_dir());
        assert!(f.git.run(&f.root, &["ls-files"]).unwrap().is_empty());
        f.git.program = f._dir.path().join("missing-git.exe").into_os_string();
        assert!(f.git.initialize(&f.root).is_err());
        assert!(f.root.join(".git").is_dir());
        let empty = f._dir.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        assert!(f.git.needs_baseline(&empty).is_err());
        assert!(!empty.join(".git").exists());
    }
}
