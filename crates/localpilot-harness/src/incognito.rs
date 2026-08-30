//! What an incognito session created, so the host can report it truthfully.
//!
//! An incognito session persists nothing of its own — its store is in-memory
//! and its closeout is skipped — but the *model* can still be granted, with an
//! explicit acknowledgement each time, permission to create a file that will
//! outlive the session. Those creations are the one durable trace, so they are
//! reported when the session ends.
//!
//! This ledger records the two things only the runtime sees exactly:
//!
//! - **Files a tool wrote outside the workspace.** A tool reports precisely
//!   what it touched (`FileTouch`), so an out-of-workspace write is exact.
//! - **Shell/background command attempts.** Recorded when presented to the
//!   permission gate (before the dispatch/cancel race), so a cancelled or
//!   timed-out command is never omitted. A command carries no contained path,
//!   so what it wrote outside the workspace cannot be attributed; the command
//!   line is recorded and the host states that scope limit rather than implying
//!   a file list it cannot produce.
//!
//! Files created *inside* the workspace are not tracked here: the host takes a
//! filesystem snapshot at the start of the session and diffs it at the end,
//! which catches shell-created files a touch cannot.

use std::path::{Path, PathBuf};

use localpilot_tools::touch::FileTouch;

/// The durable trace of an incognito session.
#[derive(Debug, Default, Clone)]
pub struct IncognitoLedger {
    tool_writes_outside_workspace: Vec<PathBuf>,
    commands: Vec<String>,
}

impl IncognitoLedger {
    /// Record every mutation a tool reported at a path outside `workspace_root`.
    /// Reads are ignored (they create nothing); in-workspace writes are left to
    /// the host's snapshot diff. Paths are de-duplicated in first-seen order.
    pub fn record_touches(&mut self, workspace_root: &Path, touches: &[FileTouch]) {
        for touch in touches {
            if !touch.op.is_mutation() {
                continue;
            }
            if path_within(&touch.path, workspace_root) {
                continue;
            }
            if !self.tool_writes_outside_workspace.contains(&touch.path) {
                self.tool_writes_outside_workspace.push(touch.path.clone());
            }
        }
    }

    /// Record a shell/background command attempt at the point it is presented to
    /// the permission gate — before the dispatch/cancel race — so a command the
    /// user cancels while it is still running, or that times out or exits
    /// nonzero, is never omitted (any of those can already have created a file).
    /// The report lists these as *attempts* (a denied or cancelled one may not
    /// have completed) rather than risk hiding a file-creating one.
    pub fn record_command(&mut self, command: impl Into<String>) {
        let command = command.into();
        if !command.is_empty() && !self.commands.contains(&command) {
            self.commands.push(command);
        }
    }

    /// Fold another ledger into this one — used to merge a delegated child
    /// session's out-of-workspace writes and approved commands into the parent's
    /// end report, so a delegation cannot hide what it created. De-duplicated in
    /// first-seen order.
    pub fn merge(&mut self, other: &IncognitoLedger) {
        for path in &other.tool_writes_outside_workspace {
            if !self.tool_writes_outside_workspace.contains(path) {
                self.tool_writes_outside_workspace.push(path.clone());
            }
        }
        for command in &other.commands {
            if !self.commands.contains(command) {
                self.commands.push(command.clone());
            }
        }
    }

    /// Files a tool wrote outside the workspace, exact.
    #[must_use]
    pub fn tool_writes_outside_workspace(&self) -> &[PathBuf] {
        &self.tool_writes_outside_workspace
    }

    /// Shell/background command attempts, verbatim — recorded when presented to
    /// the permission gate, so a cancelled or timed-out command is included.
    /// Files they created outside the workspace are not tracked — the host says so.
    #[must_use]
    pub fn commands(&self) -> &[String] {
        &self.commands
    }
}

/// Whether `path` is `root` or lives under it. Best-effort textual containment
/// over already-normalized paths (the tools report normalized paths and the
/// workspace root is absolute); a relative path is treated as in-workspace,
/// which errs toward the snapshot diff catching it rather than mis-labelling it
/// as outside.
fn path_within(path: &Path, root: &Path) -> bool {
    if path.is_relative() {
        return true;
    }
    path.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_tools::touch::TouchOp;

    fn root() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\work\project")
        } else {
            PathBuf::from("/work/project")
        }
    }

    fn under_root(rel: &str) -> PathBuf {
        root().join(rel)
    }

    fn outside(name: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\temp").join(name)
        } else {
            PathBuf::from("/tmp").join(name)
        }
    }

    #[test]
    fn only_out_of_workspace_mutations_are_recorded_and_deduped() {
        let mut ledger = IncognitoLedger::default();
        ledger.record_touches(
            &root(),
            &[
                FileTouch::whole(under_root("src/main.rs"), TouchOp::Wrote),
                FileTouch::whole(outside("a.txt"), TouchOp::Wrote),
                FileTouch::whole(outside("a.txt"), TouchOp::Modified),
                FileTouch::whole(outside("readme"), TouchOp::Read),
            ],
        );
        assert_eq!(
            ledger.tool_writes_outside_workspace(),
            &[outside("a.txt")],
            "in-workspace writes and reads are excluded; the outside write is de-duplicated"
        );
    }

    #[test]
    fn commands_are_recorded_verbatim_and_deduped() {
        let mut ledger = IncognitoLedger::default();
        ledger.record_command("echo hi > /tmp/x");
        ledger.record_command("echo hi > /tmp/x");
        ledger.record_command("");
        ledger.record_command("mkdir /tmp/y");
        assert_eq!(ledger.commands(), &["echo hi > /tmp/x", "mkdir /tmp/y"]);
    }
}
