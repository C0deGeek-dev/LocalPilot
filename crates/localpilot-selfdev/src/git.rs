//! The one place this crate shells out to `git`.
//!
//! Everything else works on the *strings* git produced, which keeps the parsing
//! testable and means a git invocation appears in exactly one place when its
//! environment needs hardening.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::SelfDevError;

/// Run `git` in `dir` and return its stdout.
///
/// `GIT_OPTIONAL_LOCKS=0` is set so a read-only query (`status`) never takes the
/// index lock: fingerprinting the tree must not race a build or an editor that
/// is also touching the repository. stdin is closed so a git that decides to
/// prompt fails fast instead of hanging a headless caller.
///
/// # Errors
/// Returns [`SelfDevError::Git`] when git cannot be spawned, exits non-zero, or
/// writes stdout that is not UTF-8.
pub(crate) fn git(dir: &Path, args: &[&str]) -> Result<String, SelfDevError> {
    let command = args.join(" ");
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| SelfDevError::Git {
            command: command.clone(),
            detail: error.to_string(),
        })?;
    if !output.status.success() {
        return Err(SelfDevError::Git {
            command,
            detail: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    String::from_utf8(output.stdout).map_err(|error| SelfDevError::Git {
        command,
        detail: error.to_string(),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::Path;
    use std::process::{Command, Stdio};

    /// A throwaway repository with a deterministic identity, so a developer's
    /// own git config (signing keys, hooks, a global `core.autocrlf`) cannot
    /// change what these tests measure.
    pub(crate) fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        run(dir.path(), &["init", "--quiet", "--initial-branch=main"]);
        run(
            dir.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        run(dir.path(), &["config", "user.name", "Test"]);
        run(dir.path(), &["config", "commit.gpgsign", "false"]);
        run(dir.path(), &["config", "core.autocrlf", "false"]);
        dir
    }

    /// Write `body` to `name` inside `root`, creating parent directories.
    pub(crate) fn write(root: &Path, name: &str, body: &str) {
        let path = root.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, body).expect("write");
    }

    /// Stage everything and commit it.
    pub(crate) fn commit_all(root: &Path, message: &str) {
        run(root, &["add", "-A"]);
        run(root, &["commit", "--quiet", "-m", message]);
    }

    fn run(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed in {dir:?}");
    }
}
