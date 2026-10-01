//! Scratch ownership is initialized by the central runtime, so every host and
//! delegated session uses the same lifecycle and model-facing authority cue.

use localpilot_sandbox::Workspace;

const OPEN: &str = "<session-scratch>";
const CLOSE: &str = "</session-scratch>";

pub(crate) fn initialize(workspace: &mut Workspace, session: &str) {
    if let Err(error) = workspace.start_scratch(session) {
        tracing::warn!(%error, "session scratch unavailable; no scratch permission granted");
    }
}

/// Host prompt replacement/append and session switching must not duplicate or
/// retain the previous session's authority cue. This block is runtime-owned.
pub(crate) fn prompt(mut text: String, workspace: &Workspace) -> String {
    while let Some(start) = text.find(OPEN) {
        let end = text[start..]
            .find(CLOSE)
            .map_or(text.len(), |end| start + end + CLOSE.len());
        text.replace_range(start..end, "");
    }
    let Some(root) = workspace.scratch_process_dir() else {
        return text.trim_end().to_string();
    };
    format!(
        "{}\n\n{OPEN}\nPrivate session scratch directory: {}\nUse for throwaway files; removed at session end. Child env LOCALPILOT_SCRATCH_DIR and TEMP/TMP/TMPDIR point here. Permission gates still apply.\n{CLOSE}",
        text.trim_end(), root.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacing_and_appending_prompts_keeps_one_current_scratch_cue() {
        let dir = tempfile::tempdir().unwrap();
        let mut ws = Workspace::new(dir.path()).unwrap();
        initialize(&mut ws, "first");
        let first = ws.scratch_process_dir().unwrap().display().to_string();
        let initial = prompt("host guidance".into(), &ws);
        let repeated = prompt(format!("{initial}\nadditional guidance\n{initial}"), &ws);
        assert_eq!(repeated.matches(OPEN).count(), 1);
        assert_eq!(repeated.matches(&first).count(), 1);
        initialize(&mut ws, "second");
        let current = prompt(repeated, &ws);
        assert_eq!(current.matches(OPEN).count(), 1);
        assert!(!current.contains(&first));
        assert!(current.contains("additional guidance"));
        ws.clear_scratch();
        assert!(!prompt(current, &ws).contains(OPEN));
    }
}
