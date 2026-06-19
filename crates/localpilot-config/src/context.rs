//! Project context-file discovery and merge.
//!
//! A project may carry free-text instruction files — `CLAUDE.md` and `AGENTS.md`
//! — at the workspace root, in nested directories, and at a per-user global
//! location (`~/.localpilot/`). This module discovers them, resolves their
//! `@`-import directives, and merges them into a single ordered [`ProjectContext`]
//! the host can inject as orientation and hand to the learning engine to ingest.
//!
//! # Precedence
//!
//! Most → least specific: **repo-root > nested directory > global**. The
//! workspace-root files are the authoritative project instructions and lead the
//! merge; nested-directory files refine within their subtree and follow; the
//! per-user global files are the baseline and come last. Within the nested tier,
//! files are ordered by ascending directory depth then path so the output is
//! deterministic across platforms.
//!
//! # `@`-imports
//!
//! A line whose trimmed text is exactly `@<path>` imports the referenced file's
//! body inline at that point. Paths resolve relative to the importing file's
//! directory (an absolute path is used as-is). Imports may nest; resolution is
//! bounded by a maximum depth and guarded against cycles, so output is always
//! finite and deterministic. A missing or unreadable import is skipped rather
//! than failing the whole discovery.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The default maximum `@`-import nesting depth. Beyond this, further imports are
/// dropped (with a marker) so a deep or adversarial chain cannot blow the stack.
pub const DEFAULT_IMPORT_DEPTH: usize = 8;

/// The default maximum directory depth the nested-file walk descends, relative to
/// the workspace root. Keeps discovery bounded on large trees.
pub const DEFAULT_DIR_DEPTH: usize = 24;

/// Which instruction file a context entry came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextKind {
    /// A `CLAUDE.md` file.
    Claude,
    /// An `AGENTS.md` file.
    Agents,
}

impl ContextKind {
    /// The on-disk file name for this kind.
    #[must_use]
    pub fn file_name(self) -> &'static str {
        match self {
            ContextKind::Claude => "CLAUDE.md",
            ContextKind::Agents => "AGENTS.md",
        }
    }

    /// All kinds, in a stable discovery order (`CLAUDE.md` before `AGENTS.md`).
    const ALL: [ContextKind; 2] = [ContextKind::Claude, ContextKind::Agents];
}

/// Where in the precedence hierarchy a context file sits. Ordered by precedence,
/// highest first: `RepoRoot` < `Nested` < `Global` (a lower value wins).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextScope {
    /// A workspace-root instruction file — the authoritative project layer.
    RepoRoot,
    /// An instruction file in a nested workspace directory.
    Nested,
    /// A per-user global instruction file under `~/.localpilot/`.
    Global,
}

/// One discovered, import-resolved context file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextFile {
    /// The file's path on disk.
    pub path: PathBuf,
    /// Which instruction file kind it is.
    pub kind: ContextKind,
    /// Its precedence scope.
    pub scope: ContextScope,
    /// Directory depth relative to the workspace root (0 = root; usize::MAX for a
    /// global file, which has no workspace depth).
    pub depth: usize,
    /// The file body, with `@`-imports resolved inline.
    pub body: String,
}

/// The merged project context: every discovered context file in precedence order
/// (highest-precedence first).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectContext {
    /// The discovered files, ordered by precedence (repo-root, then nested by
    /// ascending depth/path, then global).
    pub files: Vec<ContextFile>,
}

impl ProjectContext {
    /// Whether any context file was discovered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Render the merged instruction text: each file's body in precedence order,
    /// separated by a labelled header so provenance survives the merge. Returns an
    /// empty string when no files were discovered.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        for file in &self.files {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&format!(
                "<!-- {} ({}) -->\n",
                file.path.display(),
                scope_label(file.scope)
            ));
            out.push_str(file.body.trim_end());
            out.push('\n');
        }
        out
    }
}

fn scope_label(scope: ContextScope) -> &'static str {
    match scope {
        ContextScope::RepoRoot => "repo-root",
        ContextScope::Nested => "nested",
        ContextScope::Global => "global",
    }
}

/// Discovers and merges a project's context files.
///
/// Construct with [`ContextDiscovery::new`] for the real, env-resolved global
/// location, or supply an explicit global directory with
/// [`ContextDiscovery::with_global_dir`] (used in tests so global discovery does
/// not depend on the host's home directory).
#[derive(Debug, Clone)]
pub struct ContextDiscovery {
    workspace_root: PathBuf,
    global_dir: Option<PathBuf>,
    max_import_depth: usize,
    max_dir_depth: usize,
}

impl ContextDiscovery {
    /// A discovery rooted at `workspace_root`, with the global location resolved
    /// from the user's home directory (`~/.localpilot/`).
    #[must_use]
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            global_dir: global_context_dir(),
            max_import_depth: DEFAULT_IMPORT_DEPTH,
            max_dir_depth: DEFAULT_DIR_DEPTH,
        }
    }

    /// Override the global context directory (or disable it with `None`).
    #[must_use]
    pub fn with_global_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.global_dir = dir;
        self
    }

    /// Override the maximum `@`-import nesting depth.
    #[must_use]
    pub fn with_max_import_depth(mut self, depth: usize) -> Self {
        self.max_import_depth = depth;
        self
    }

    /// Override the maximum nested-directory walk depth.
    #[must_use]
    pub fn with_max_dir_depth(mut self, depth: usize) -> Self {
        self.max_dir_depth = depth;
        self
    }

    /// Discover, import-resolve, and merge the project's context files. Best-effort
    /// and non-fatal: an unreadable file or walk error is skipped, never returned
    /// as an error, so a partially-readable tree still yields the context it can.
    #[must_use]
    pub fn discover(&self) -> ProjectContext {
        let mut files: Vec<ContextFile> = Vec::new();

        // Repo-root layer: the workspace-root instruction files.
        for kind in ContextKind::ALL {
            let path = self.workspace_root.join(kind.file_name());
            if let Some(file) = self.load(&path, kind, ContextScope::RepoRoot, 0) {
                files.push(file);
            }
        }

        // Nested layer: instruction files in subdirectories of the workspace.
        files.extend(self.discover_nested());

        // Global layer: per-user instruction files under `~/.localpilot/`.
        if let Some(global_dir) = &self.global_dir {
            for kind in ContextKind::ALL {
                let path = global_dir.join(kind.file_name());
                if let Some(file) = self.load(&path, kind, ContextScope::Global, usize::MAX) {
                    files.push(file);
                }
            }
        }

        files.sort_by(|a, b| {
            a.scope
                .cmp(&b.scope)
                .then(a.depth.cmp(&b.depth))
                .then_with(|| a.path.cmp(&b.path))
        });

        ProjectContext { files }
    }

    /// Walk the workspace tree (honouring ignore files) for instruction files in
    /// nested directories. Root-level files are handled by the repo-root layer and
    /// excluded here.
    fn discover_nested(&self) -> Vec<ContextFile> {
        let mut out = Vec::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let walker = ignore::WalkBuilder::new(&self.workspace_root)
            .max_depth(Some(self.max_dir_depth))
            .hidden(false)
            .build();
        for entry in walker.flatten() {
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(kind) = ContextKind::ALL.into_iter().find(|k| k.file_name() == name) else {
                continue;
            };
            let depth = self.workspace_depth(path);
            // Depth 0 is the repo-root layer, already collected.
            if depth == 0 {
                continue;
            }
            if !seen.insert(path.to_path_buf()) {
                continue;
            }
            if let Some(file) = self.load(path, kind, ContextScope::Nested, depth) {
                out.push(file);
            }
        }
        out
    }

    /// Directory depth of `path`'s parent relative to the workspace root.
    fn workspace_depth(&self, path: &Path) -> usize {
        path.parent()
            .and_then(|parent| parent.strip_prefix(&self.workspace_root).ok())
            .map(|rel| rel.components().count())
            .unwrap_or(0)
    }

    /// Read and import-resolve one context file. Returns `None` when the file does
    /// not exist or cannot be read.
    fn load(
        &self,
        path: &Path,
        kind: ContextKind,
        scope: ContextScope,
        depth: usize,
    ) -> Option<ContextFile> {
        if !path.is_file() {
            return None;
        }
        let raw = std::fs::read_to_string(path).ok()?;
        let mut visiting = HashSet::new();
        // The root file counts as the first level of the import budget.
        visiting.insert(canonical(path));
        let body = self.resolve_imports(&raw, path, &mut visiting, 0);
        Some(ContextFile {
            path: path.to_path_buf(),
            kind,
            scope,
            depth,
            body,
        })
    }

    /// Resolve `@<path>` import directives in `text`, inlining each referenced
    /// file's (recursively resolved) body. `base` is the importing file's path;
    /// relative import paths resolve against its directory. `visiting` holds the
    /// canonical paths on the current chain for cycle detection; `depth` is the
    /// current nesting level.
    fn resolve_imports(
        &self,
        text: &str,
        base: &Path,
        visiting: &mut HashSet<PathBuf>,
        depth: usize,
    ) -> String {
        let mut out = String::new();
        for line in text.lines() {
            match import_target(line) {
                Some(target) => {
                    let resolved = resolve_against(base, target);
                    let canon = canonical(&resolved);
                    if depth + 1 > self.max_import_depth {
                        out.push_str(&format!("<!-- import skipped (max depth): {target} -->\n"));
                        continue;
                    }
                    if visiting.contains(&canon) {
                        out.push_str(&format!("<!-- import skipped (cycle): {target} -->\n"));
                        continue;
                    }
                    let Ok(imported) = std::fs::read_to_string(&resolved) else {
                        out.push_str(&format!("<!-- import not found: {target} -->\n"));
                        continue;
                    };
                    visiting.insert(canon.clone());
                    let nested = self.resolve_imports(&imported, &resolved, visiting, depth + 1);
                    visiting.remove(&canon);
                    out.push_str(nested.trim_end());
                    out.push('\n');
                }
                None => {
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
        out
    }
}

/// If `line` is an import directive (`@<path>` after trimming, and nothing else),
/// return the path token. A line with prose around an `@` is not a directive.
fn import_target(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix('@')?;
    if rest.is_empty() || rest.contains(char::is_whitespace) {
        return None;
    }
    Some(rest)
}

/// Resolve an import `target` against the importing file `base`: an absolute path
/// is used as-is, a relative one against `base`'s parent directory.
fn resolve_against(base: &Path, target: &str) -> PathBuf {
    let target_path = Path::new(target);
    if target_path.is_absolute() {
        target_path.to_path_buf()
    } else {
        base.parent()
            .unwrap_or_else(|| Path::new("."))
            .join(target_path)
    }
}

/// A best-effort canonical key for cycle detection: the canonicalized path when
/// available, else the path as given. Canonicalization collapses `.`/`..` and
/// symlinks so two spellings of the same file are detected as one.
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The per-user global context directory, `~/.localpilot/`, resolved
/// cross-platform from the home directory. `None` when no home is set.
fn global_context_dir() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".localpilot"))
}

#[cfg(windows)]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

#[cfg(not(windows))]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::fs;

    /// A discovery over `root` with the global layer pointed at `global` (or
    /// disabled when `None`), so tests never depend on the host's home directory.
    fn discovery(root: &Path, global: Option<&Path>) -> ContextDiscovery {
        ContextDiscovery::new(root).with_global_dir(global.map(Path::to_path_buf))
    }

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    #[test]
    fn precedence_orders_repo_root_then_nested_then_global() {
        let ws = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        write(&ws.path().join("CLAUDE.md"), "root rules");
        write(&ws.path().join("sub").join("CLAUDE.md"), "nested rules");
        write(&global.path().join("CLAUDE.md"), "global rules");

        let ctx = discovery(ws.path(), Some(global.path())).discover();
        let scopes: Vec<_> = ctx.files.iter().map(|f| f.scope).collect();
        assert_eq!(
            scopes,
            vec![
                ContextScope::RepoRoot,
                ContextScope::Nested,
                ContextScope::Global
            ]
        );

        // The merge renders highest-precedence first.
        let rendered = ctx.render();
        let root_at = rendered.find("root rules").unwrap();
        let nested_at = rendered.find("nested rules").unwrap();
        let global_at = rendered.find("global rules").unwrap();
        assert!(root_at < nested_at && nested_at < global_at, "{rendered}");
    }

    #[test]
    fn discovers_both_claude_and_agents_at_root() {
        let ws = tempfile::tempdir().unwrap();
        write(&ws.path().join("CLAUDE.md"), "claude");
        write(&ws.path().join("AGENTS.md"), "agents");

        let ctx = discovery(ws.path(), None).discover();
        let kinds: Vec<_> = ctx.files.iter().map(|f| f.kind).collect();
        assert!(kinds.contains(&ContextKind::Claude));
        assert!(kinds.contains(&ContextKind::Agents));
    }

    #[test]
    fn nested_files_order_by_ascending_depth_then_path() {
        let ws = tempfile::tempdir().unwrap();
        write(&ws.path().join("a").join("b").join("CLAUDE.md"), "deep");
        write(&ws.path().join("z").join("CLAUDE.md"), "shallow-z");
        write(&ws.path().join("a").join("CLAUDE.md"), "shallow-a");

        let ctx = discovery(ws.path(), None).discover();
        let bodies: Vec<_> = ctx
            .files
            .iter()
            .map(|f| f.body.trim().to_string())
            .collect();
        // Depth 1 before depth 2; within depth 1, path order (a before z).
        assert_eq!(bodies, vec!["shallow-a", "shallow-z", "deep"]);
    }

    #[test]
    fn at_import_inlines_referenced_file() {
        let ws = tempfile::tempdir().unwrap();
        write(&ws.path().join("shared.md"), "shared body");
        write(&ws.path().join("CLAUDE.md"), "before\n@shared.md\nafter");

        let ctx = discovery(ws.path(), None).discover();
        let root = ctx
            .files
            .iter()
            .find(|f| f.scope == ContextScope::RepoRoot)
            .unwrap();
        assert!(root.body.contains("shared body"), "{}", root.body);
        assert!(root.body.contains("before") && root.body.contains("after"));
        // The directive itself is consumed, not echoed.
        assert!(!root.body.contains("@shared.md"), "{}", root.body);
    }

    #[test]
    fn prose_at_sign_is_not_an_import() {
        let ws = tempfile::tempdir().unwrap();
        write(&ws.path().join("CLAUDE.md"), "ping @someone in the channel");
        let ctx = discovery(ws.path(), None).discover();
        assert!(ctx.files[0].body.contains("@someone"));
    }

    #[test]
    fn import_cycle_is_broken_and_marked() {
        let ws = tempfile::tempdir().unwrap();
        write(&ws.path().join("a.md"), "A\n@b.md");
        write(&ws.path().join("b.md"), "B\n@a.md");
        write(&ws.path().join("CLAUDE.md"), "@a.md");

        // Finite output (no stack overflow / hang) with a cycle marker.
        let ctx = discovery(ws.path(), None).discover();
        let body = &ctx.files[0].body;
        assert!(body.contains('A') && body.contains('B'));
        assert!(body.contains("cycle"), "{body}");
    }

    #[test]
    fn import_depth_is_bounded() {
        let ws = tempfile::tempdir().unwrap();
        // A chain c0 -> c1 -> c2 -> c3, with a tight import budget of 1.
        write(&ws.path().join("c3.md"), "deepest");
        write(&ws.path().join("c2.md"), "two\n@c3.md");
        write(&ws.path().join("c1.md"), "one\n@c2.md");
        write(&ws.path().join("CLAUDE.md"), "zero\n@c1.md");

        let ctx = discovery(ws.path(), None)
            .with_max_import_depth(1)
            .discover();
        let body = &ctx.files[0].body;
        assert!(body.contains("one"), "{body}"); // depth-1 import resolved
        assert!(body.contains("max depth"), "{body}"); // the next level is cut
        assert!(!body.contains("deepest"), "{body}");
    }

    #[test]
    fn missing_import_is_tolerated() {
        let ws = tempfile::tempdir().unwrap();
        write(&ws.path().join("CLAUDE.md"), "keep\n@nope.md");
        let ctx = discovery(ws.path(), None).discover();
        let body = &ctx.files[0].body;
        assert!(body.contains("keep"));
        assert!(body.contains("not found"), "{body}");
    }

    #[test]
    fn empty_workspace_yields_empty_context() {
        let ws = tempfile::tempdir().unwrap();
        let ctx = discovery(ws.path(), None).discover();
        assert!(ctx.is_empty());
        assert!(ctx.render().is_empty());
    }

    #[test]
    fn nested_paths_resolve_across_directory_separators() {
        // Cross-platform: a multi-segment nested path resolves and reports its
        // depth regardless of the OS path separator (the join builds the native
        // separator; the depth is component-counted, not separator-counted).
        let ws = tempfile::tempdir().unwrap();
        let nested = ws.path().join("src").join("inner").join("AGENTS.md");
        write(&nested, "deep agents");
        let ctx = discovery(ws.path(), None).discover();
        let file = ctx
            .files
            .iter()
            .find(|f| f.kind == ContextKind::Agents)
            .unwrap();
        assert_eq!(file.scope, ContextScope::Nested);
        assert_eq!(file.depth, 2);
    }
}
