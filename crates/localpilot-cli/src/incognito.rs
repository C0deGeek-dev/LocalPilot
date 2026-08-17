//! Reporting what an incognito session left on disk.
//!
//! An incognito session persists none of its *own* records, but the model can
//! be granted — with an acknowledgement each time — permission to create a file
//! that outlives the session. Reporting those honestly needs three sources,
//! each with a different exactness, and this module keeps them distinct rather
//! than pretending to one perfect list:
//!
//! 1. **Files created under the workspace.** A filesystem snapshot is taken
//!    when the session starts and diffed when it ends, with **no ignore
//!    filtering** — a file written under `target/` or `node_modules/` counts,
//!    which a `.gitignore`-aware walk would miss. Version-control internals
//!    under `.git/` churn on their own and are collapsed to a single count.
//! 2. **Files a tool wrote outside the workspace.** Taken from the runtime's
//!    ledger, where a tool reported exactly what it touched.
//! 3. **Approved shell/background commands.** Listed verbatim, because a command
//!    carries no contained path: what it wrote *outside* the workspace cannot be
//!    attributed, and the report says so instead of implying a file list it
//!    cannot produce.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use localpilot_harness::IncognitoLedger;
use localpilot_slash::SlashAction;

/// The refusal message for a persistent slash command under incognito, or
/// `None` when the action writes nothing durable and may run.
#[must_use]
pub fn incognito_refusal(action: &SlashAction) -> Option<String> {
    action
        .persistence()
        .persistent_target()
        .map(|what| format!("not available in incognito — it would {what}"))
}

/// A record of which files exist under a workspace at one instant.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceSnapshot {
    /// Workspace-relative paths of ordinary files (everything except `.git/`).
    files: BTreeSet<PathBuf>,
    /// How many files live under any `.git/` directory, counted rather than
    /// listed — version-control internals are noise in this report.
    git_files: usize,
}

impl WorkspaceSnapshot {
    /// Walk `root` and record every file, with **no** ignore filtering. A path
    /// that cannot be read is skipped (best-effort — the report is advisory).
    #[must_use]
    pub fn take(root: &Path) -> Self {
        let mut snapshot = WorkspaceSnapshot::default();
        snapshot.walk(root, root);
        snapshot
    }

    fn walk(&mut self, root: &Path, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            // Do not follow symlinks: a link into a huge tree (or a cycle) must
            // not stall session close, and a link is not a file this session
            // created under the workspace.
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if is_git_dir(&path) {
                    self.git_files += count_files(&path);
                } else {
                    self.walk(root, &path);
                }
            } else if file_type.is_file() {
                if let Ok(rel) = path.strip_prefix(root) {
                    self.files.insert(rel.to_path_buf());
                }
            }
        }
    }
}

/// A count of files created between two snapshots.
#[derive(Debug, Clone, Default)]
pub struct IncognitoReport {
    /// Files that appeared under the workspace, relative to its root.
    workspace_created: Vec<PathBuf>,
    /// Net new files under `.git/` (collapsed count).
    git_new: usize,
    /// Files a tool wrote at absolute paths outside the workspace.
    tool_writes_outside: Vec<PathBuf>,
    /// Approved shell/background commands, verbatim.
    commands: Vec<String>,
}

impl IncognitoReport {
    /// Assemble the report from the before/after workspace snapshots and the
    /// runtime's ledger.
    #[must_use]
    pub fn assemble(
        before: &WorkspaceSnapshot,
        after: &WorkspaceSnapshot,
        ledger: &IncognitoLedger,
    ) -> Self {
        let workspace_created: Vec<PathBuf> =
            after.files.difference(&before.files).cloned().collect();
        IncognitoReport {
            workspace_created,
            git_new: after.git_files.saturating_sub(before.git_files),
            tool_writes_outside: ledger.tool_writes_outside_workspace().to_vec(),
            commands: ledger.commands().to_vec(),
        }
    }

    /// Whether the session created nothing and ran no command — the report is
    /// then a single reassuring line rather than empty sections.
    #[must_use]
    pub fn is_quiet(&self) -> bool {
        self.workspace_created.is_empty()
            && self.git_new == 0
            && self.tool_writes_outside.is_empty()
            && self.commands.is_empty()
    }

    /// Render the report as plain lines for stderr (and the same text a UI
    /// panel can show). `root` labels the workspace section.
    #[must_use]
    pub fn render(&self, root: &Path) -> String {
        let mut out = String::new();
        out.push_str(
            "incognito session ended — nothing was saved to session memory or LocalMind.\n",
        );
        if self.is_quiet() {
            out.push_str("no files were created and no shell commands ran.\n");
            return out;
        }

        if self.workspace_created.is_empty() && self.git_new == 0 {
            out.push_str("no files were created under the workspace.\n");
        } else {
            out.push_str(&format!(
                "files created under the workspace ({}):\n",
                root.display()
            ));
            for path in &self.workspace_created {
                out.push_str(&format!("  {}\n", path.display()));
            }
            if self.git_new > 0 {
                out.push_str(&format!(
                    "  {} file(s) under .git/ (version-control internals)\n",
                    self.git_new
                ));
            }
        }

        if !self.tool_writes_outside.is_empty() {
            out.push_str("files a tool wrote outside the workspace:\n");
            for path in &self.tool_writes_outside {
                out.push_str(&format!("  {}\n", path.display()));
            }
        }

        if !self.commands.is_empty() {
            out.push_str(
                "shell/background command attempts presented to the permission gate (a denied or \
                 cancelled one may not have completed; files any created outside the workspace are \
                 not tracked):\n",
            );
            for command in &self.commands {
                out.push_str(&format!("  $ {command}\n"));
            }
        }
        out
    }
}

/// Whether a directory is a `.git` version-control directory.
fn is_git_dir(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()) == Some(".git")
}

/// Count every file under `dir`, recursively. Best-effort.
fn count_files(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut count = 0;
    for entry in entries.flatten() {
        match entry.file_type() {
            Ok(t) if t.is_dir() => count += count_files(&entry.path()),
            Ok(t) if t.is_file() => count += 1,
            _ => {}
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_slash::{IngestAction, Profile};

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, b"x").unwrap();
    }

    #[test]
    fn a_persistent_command_is_refused_with_a_reason_and_a_read_is_not() {
        let refusal = incognito_refusal(&SlashAction::Ingest(IngestAction::Run));
        assert!(refusal.is_some_and(|m| m.contains("not available in incognito")));
        assert!(incognito_refusal(&SlashAction::Tree).is_none());
        assert!(incognito_refusal(&SlashAction::SetProfile(Profile::Bypass)).is_none());
    }

    #[test]
    fn the_report_lists_workspace_files_including_normally_ignored_ones() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("src/main.rs"));
        let before = WorkspaceSnapshot::take(root);

        // A build artifact under target/ and a source file appear; both count,
        // because there is no ignore filtering.
        touch(&root.join("target/debug/out.bin"));
        touch(&root.join("notes.md"));
        let after = WorkspaceSnapshot::take(root);

        let report = IncognitoReport::assemble(&before, &after, &IncognitoLedger::default());
        assert!(!report.is_quiet());
        let mut created: Vec<String> = report
            .workspace_created
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        created.sort();
        assert_eq!(created, vec!["notes.md", "target/debug/out.bin"]);
        let text = report.render(root);
        assert!(text.contains("target"), "{text}");
        assert!(text.contains("nothing was saved"), "{text}");
    }

    #[test]
    fn git_internals_are_counted_not_listed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let before = WorkspaceSnapshot::take(root);
        touch(&root.join(".git/objects/ab/cdef"));
        touch(&root.join(".git/index"));
        let after = WorkspaceSnapshot::take(root);

        let report = IncognitoReport::assemble(&before, &after, &IncognitoLedger::default());
        assert!(report.workspace_created.is_empty());
        assert_eq!(report.git_new, 2);
        assert!(report.render(root).contains("version-control internals"));
    }

    #[test]
    fn a_quiet_session_reports_that_nothing_was_created() {
        let dir = tempfile::tempdir().unwrap();
        let snap = WorkspaceSnapshot::take(dir.path());
        let report = IncognitoReport::assemble(&snap, &snap, &IncognitoLedger::default());
        assert!(report.is_quiet());
        let text = report.render(dir.path());
        assert!(
            text.contains("no files were created and no shell commands ran"),
            "{text}"
        );
    }
}
