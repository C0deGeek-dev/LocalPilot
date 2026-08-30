//! The set of workspace files a session has touched — the "files in play".
//!
//! Path-scoped instruction files (`.github/instructions/*.instructions.md`)
//! carry an `applyTo` glob and reach the model only when a file they are about
//! is actually involved. Which files those are is a per-turn fact, not a
//! discovery-time one, so the runtime records every workspace path a tool call
//! names and the pre-turn context hook reads that set back.
//!
//! The handle is cheap to clone and shared between the runtime (which records)
//! and the hook (which reads), because the hook is registered as an `Arc` and
//! sees only `&self`.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// A shared, session-scoped set of workspace-relative paths in play. Bounded so
/// a long session cannot grow it without limit; the cap is generous relative to
/// the number of distinct files one session touches.
#[derive(Debug, Clone, Default)]
pub struct PathsInPlay {
    paths: Arc<Mutex<BTreeSet<String>>>,
}

/// The most paths tracked per session. Past this the set stops growing: an
/// instruction whose glob matches nothing in the first thousand files touched
/// is not the case this feature exists for.
const MAX_PATHS: usize = 1024;

impl PathsInPlay {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `path` (absolute or relative) as in play, stored relative to
    /// `root` with `/` separators so glob matching is platform-independent. A
    /// path outside the workspace is ignored — an instruction file's glob is
    /// about the project's own layout.
    pub fn record(&self, root: &Path, path: &Path) {
        let Some(relative) = relative_to(root, path) else {
            return;
        };
        if let Ok(mut paths) = self.paths.lock() {
            if paths.len() < MAX_PATHS {
                paths.insert(relative);
            }
        }
    }

    /// The recorded paths, in stable order.
    #[must_use]
    pub fn snapshot(&self) -> Vec<String> {
        self.paths
            .lock()
            .map(|paths| paths.iter().cloned().collect())
            .unwrap_or_default()
    }
}

/// `path` expressed relative to `root` with forward slashes, or `None` when it
/// does not live under `root`.
fn relative_to(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let text = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn records_workspace_relative_paths_with_forward_slashes() {
        let root = Path::new("/work/repo");
        let paths = PathsInPlay::new();
        paths.record(root, &Path::new("/work/repo").join("src").join("app.ts"));
        assert_eq!(paths.snapshot(), vec!["src/app.ts".to_string()]);
    }

    #[test]
    fn a_path_outside_the_workspace_is_ignored() {
        let paths = PathsInPlay::new();
        paths.record(Path::new("/work/repo"), Path::new("/etc/hosts"));
        assert!(paths.snapshot().is_empty());
    }

    #[test]
    fn the_set_is_bounded() {
        let root = Path::new("/work/repo");
        let paths = PathsInPlay::new();
        for i in 0..(MAX_PATHS + 50) {
            paths.record(root, &root.join(format!("f{i}.rs")));
        }
        assert_eq!(paths.snapshot().len(), MAX_PATHS);
    }
}
