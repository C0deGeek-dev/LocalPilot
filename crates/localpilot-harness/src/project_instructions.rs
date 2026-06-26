//! Direct injection of a project's instruction files into the turn context.
//!
//! A project's `Navigator.md` / `CLAUDE.md` / `AGENTS.md` /
//! `.github/copilot-instructions.md` are the user's authoritative orientation for
//! the agent. They reach the model through the review-gated learning store only
//! after a human accepts them, which means a fresh project's instructions may
//! never reach the model. This hook injects the merged instruction text directly
//! into the turn context every turn — bounded and redacted, but **ungated** — so
//! a checkout's instructions are respected immediately, independent of learning.
//!
//! It reuses [`ContextDiscovery`] (precedence, `@`-imports, nested + global tiers)
//! rather than re-walking the tree, and is computed once per session: the
//! instruction files do not change mid-session, so discovery runs at construction
//! and each turn returns the cached, bounded text.

use std::path::Path;
use std::sync::Arc;

use localpilot_config::redact;
use localpilot_config::ContextDiscovery;

use crate::{ContextHook, SessionRuntime};

/// A context hook that contributes the merged, bounded, redacted project
/// instruction text before each turn.
pub struct ProjectInstructionsContext {
    /// The injected block, or `None` when the project carries no instruction
    /// files. Computed once at construction.
    block: Option<String>,
}

impl ProjectInstructionsContext {
    /// Discover, merge, redact, and bound the project's instruction files rooted
    /// at `root`, capping the injected text at `char_budget` characters.
    #[must_use]
    pub fn new(root: &Path, char_budget: usize) -> Self {
        let rendered = ContextDiscovery::new(root).discover().render();
        let block = if rendered.trim().is_empty() {
            None
        } else {
            let redacted = redact::redact(&rendered);
            let bounded = bound_with_marker(&redacted, char_budget);
            Some(format!(
                "Project instructions (authoritative — follow these):\n{bounded}"
            ))
        };
        Self { block }
    }

    /// Whether any instruction text was discovered (and so the hook is worth
    /// registering).
    #[must_use]
    pub fn has_instructions(&self) -> bool {
        self.block.is_some()
    }
}

impl ContextHook for ProjectInstructionsContext {
    fn name(&self) -> &str {
        "project-instructions"
    }

    fn context_for(&self, _prompt: &str) -> Option<String> {
        self.block.clone()
    }
}

/// Register the project-instructions context hook on `runtime` when enabled and
/// the project actually carries instruction files. Discovery runs once here.
pub fn register_project_instructions_context(
    root: &Path,
    enabled: bool,
    char_budget: usize,
    runtime: &mut SessionRuntime,
) {
    if !enabled {
        return;
    }
    let hook = ProjectInstructionsContext::new(root, char_budget);
    if hook.has_instructions() {
        runtime.hooks_mut().register_context_hook(Arc::new(hook));
    }
}

/// Truncate `text` to `budget` characters on a char boundary, appending a marker
/// so an over-budget instruction set is visibly truncated rather than silently
/// dropped. A `0` budget injects nothing but the marker.
fn bound_with_marker(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_string();
    }
    let mut end = budget;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = text[..end].to_string();
    out.push_str(&format!(
        "\n<!-- project instructions truncated at {budget} chars -->"
    ));
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn no_block_when_workspace_has_no_instruction_files() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ProjectInstructionsContext::new(dir.path(), 8_000);
        assert!(!ctx.has_instructions());
        assert!(ctx.context_for("anything").is_none());
    }

    #[test]
    fn injects_claude_md_text() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "use four-space indent").unwrap();
        let ctx = ProjectInstructionsContext::new(dir.path(), 8_000);
        let block = ctx.context_for("hi").expect("an instruction block");
        assert!(block.contains("use four-space indent"));
        assert!(block.contains("authoritative"));
    }

    #[test]
    fn over_budget_text_is_truncated_with_a_marker() {
        let dir = tempfile::tempdir().unwrap();
        let big = "x".repeat(5_000);
        std::fs::write(dir.path().join("CLAUDE.md"), &big).unwrap();
        let ctx = ProjectInstructionsContext::new(dir.path(), 500);
        let block = ctx.context_for("hi").unwrap();
        assert!(block.contains("truncated at 500 chars"));
        // Bounded well under the raw size (budget + header + marker).
        assert!(block.len() < 1_000, "len {}", block.len());
    }

    #[test]
    fn secrets_in_instructions_are_redacted_before_injection() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("AGENTS.md"),
            "deploy with sk-ant-api03-AAAABBBBCCCCDDDDEEEEFFFFGGGGHHHHIIIIJJJJKKKKLLLL",
        )
        .unwrap();
        let block = ProjectInstructionsContext::new(dir.path(), 8_000)
            .context_for("hi")
            .unwrap();
        assert!(
            !block.contains("sk-ant-api03-AAAABBBBCCCCDDDD"),
            "a secret-shaped token must be redacted: {block}"
        );
    }
}
