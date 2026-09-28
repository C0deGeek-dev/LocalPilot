//! Which working tree a mesh command operates on: `--repo`, then
//! `PAIR_REPO`, then the current directory, each resolved to its Git top
//! level or, without Git, to the nearest directory holding a mailbox. Shared
//! by `localpilot mesh` and `doctor`, so both name the same tree.

use std::path::{Path, PathBuf};

use crate::layout::MAILBOX_DIR;

/// The anchor tree and how it was chosen (`flag`, `env` or `cwd`).
///
/// # Errors
/// A message naming the problem: a `PAIR_REPO` that is not a directory or
/// has no mailbox (never a fallback to the current directory), or a start
/// with neither Git nor a mailbox.
pub fn resolve(repo: Option<&Path>) -> Result<(PathBuf, &'static str), String> {
    let (start, source) = if let Some(r) = repo {
        (r.to_path_buf(), "flag")
    } else if let Some(env) = std::env::var_os("PAIR_REPO").filter(|v| !v.is_empty()) {
        let p = PathBuf::from(&env);
        if !p.is_dir() {
            return Err(format!(
                "PAIR_REPO={} is not a directory; fix or unset it (no fallback to the current directory)",
                p.display()
            ));
        }
        (p, "env")
    } else {
        let cwd = std::env::current_dir().map_err(|e| format!("no current directory: {e}"))?;
        (cwd, "cwd")
    };
    let start = dunce::canonicalize(&start)
        .map_err(|e| format!("cannot resolve {}: {e}", start.display()))?;
    if let Some(top) = git_toplevel(&start) {
        return Ok((top, source));
    }
    for dir in start.ancestors() {
        if dir.join(MAILBOX_DIR).is_dir() {
            return Ok((dir.to_path_buf(), source));
        }
    }
    if source == "env" {
        return Err(format!(
            "PAIR_REPO={} has no pair mailbox; fix or unset it (no fallback to the current directory)",
            start.display()
        ));
    }
    Err(format!(
        "no Git repository and no pair mailbox at {}; point --repo at the session's working tree",
        start.display()
    ))
}

fn git_toplevel(dir: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let top = String::from_utf8(out.stdout).ok()?;
    dunce::canonicalize(top.trim()).ok()
}
