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
//! rather than re-walking the tree. Discovery runs once at construction: the
//! instruction files do not change mid-session.
//!
//! Path-scoped files (`.github/instructions/*.instructions.md`) are the one part
//! that is *not* fixed for the session. Their `applyTo` glob decides per turn
//! whether the rule is relevant, matched against the files in play, so the
//! rendered block is recomputed each turn while the unscoped part is stable.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use localpilot_config::redact;
use localpilot_config::{ContextDiscovery, ProjectContext};

use crate::{ContextHook, PathsInPlay, SessionRuntime};

/// A context hook that contributes the merged, bounded, redacted project
/// instruction text before each turn.
pub struct ProjectInstructionsContext {
    /// The discovered instruction files. Discovery runs once at construction.
    context: ProjectContext,
    /// The workspace root, for resolving prompt-named paths.
    root: PathBuf,
    /// The files this session has touched, consulted per turn so a path-scoped
    /// rule reaches the model exactly when its glob matches something in play.
    paths_in_play: PathsInPlay,
    /// The character cap on the injected block.
    char_budget: usize,
}

impl ProjectInstructionsContext {
    /// Discover and merge the project's instruction files rooted at `root`,
    /// capping the injected text at `char_budget` characters.
    #[must_use]
    pub fn new(root: &Path, char_budget: usize) -> Self {
        Self::with_paths_in_play(root, char_budget, PathsInPlay::new())
    }

    /// [`ProjectInstructionsContext::new`] sharing the session's set of files in
    /// play, so path-scoped instructions can be matched against what the session
    /// has actually touched.
    #[must_use]
    pub fn with_paths_in_play(root: &Path, char_budget: usize, paths_in_play: PathsInPlay) -> Self {
        Self {
            context: ContextDiscovery::new(root).discover(),
            root: root.to_path_buf(),
            paths_in_play,
            char_budget,
        }
    }

    /// Whether any instruction text was discovered (and so the hook is worth
    /// registering).
    #[must_use]
    pub fn has_instructions(&self) -> bool {
        !self.context.is_empty()
    }

    /// The paths a turn is about: everything the session has touched, plus any
    /// workspace file the prompt names outright. A prompt like "fix the types in
    /// src/app.ts" should reach a `**/*.ts` rule on the first turn, before any
    /// tool has run.
    fn paths_for(&self, prompt: &str) -> Vec<String> {
        let mut paths = self.paths_in_play.snapshot();
        for candidate in prompt_path_candidates(prompt) {
            if self.root.join(&candidate).is_file() && !paths.contains(&candidate) {
                paths.push(candidate);
            }
        }
        paths
    }
}

/// Path-shaped tokens in a prompt: whitespace-separated words carrying a `/` or
/// a `.`, stripped of surrounding punctuation and normalized to `/` separators.
/// Deliberately cheap and conservative — each candidate is confirmed against the
/// workspace before it counts, so a false positive costs one `is_file` call.
fn prompt_path_candidates(prompt: &str) -> Vec<String> {
    const MAX_CANDIDATES: usize = 32;
    prompt
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|c: char| {
                !c.is_alphanumeric() && !matches!(c, '/' | '\\' | '.' | '_' | '-')
            })
            .replace('\\', "/")
        })
        .filter(|word| {
            !word.is_empty() && word.contains('.') && !word.starts_with('/') && word.len() < 200
        })
        .take(MAX_CANDIDATES)
        .collect()
}

impl ContextHook for ProjectInstructionsContext {
    fn name(&self) -> &str {
        "project-instructions"
    }

    fn context_for(&self, prompt: &str) -> Option<String> {
        let rendered = self.context.render_for(&self.paths_for(prompt));
        if rendered.trim().is_empty() {
            return None;
        }
        let redacted = redact::redact(&rendered);
        let bounded = bound_with_marker(&redacted, self.char_budget);
        Some(format!(
            "Project instructions (authoritative — follow these):\n{bounded}"
        ))
    }
}

/// Register the project-instructions context hook on `runtime` when enabled and
/// the project actually carries instruction files. Discovery runs once here; the
/// hook shares the runtime's set of files in play so path-scoped instructions
/// track what the session touches.
pub fn register_project_instructions_context(
    root: &Path,
    enabled: bool,
    char_budget: usize,
    runtime: &mut SessionRuntime,
) {
    if !enabled {
        return;
    }
    let hook =
        ProjectInstructionsContext::with_paths_in_play(root, char_budget, runtime.paths_in_play());
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

    fn write(root: &Path, rel: &str, contents: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

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

    #[test]
    fn a_scoped_rule_waits_for_a_matching_file_to_be_in_play() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "CLAUDE.md", "always applies");
        write(
            dir.path(),
            ".github/instructions/ts.instructions.md",
            "---\napplyTo: \"**/*.ts\"\n---\nPrefer named exports.",
        );
        write(dir.path(), "src/app.ts", "export const x = 1;");
        write(dir.path(), "src/main.rs", "fn main() {}");

        let paths = PathsInPlay::new();
        let ctx = ProjectInstructionsContext::with_paths_in_play(dir.path(), 8_000, paths.clone());

        // Nothing in play: the general instructions inject, the scoped one does not.
        let block = ctx.context_for("what does this project do?").unwrap();
        assert!(block.contains("always applies"));
        assert!(!block.contains("Prefer named exports"));

        // A Rust file in play still does not match a `**/*.ts` rule.
        paths.record(dir.path(), &dir.path().join("src").join("main.rs"));
        let block = ctx.context_for("fix the build").unwrap();
        assert!(!block.contains("Prefer named exports"));

        // A TypeScript file does.
        paths.record(dir.path(), &dir.path().join("src").join("app.ts"));
        let block = ctx.context_for("fix the build").unwrap();
        assert!(block.contains("Prefer named exports"));
        assert!(block.contains("always applies"));
    }

    #[test]
    fn a_prompt_naming_a_file_puts_it_in_play_on_the_first_turn() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            ".github/instructions/ts.instructions.md",
            "---\napplyTo: '**/*.ts'\n---\nPrefer named exports.",
        );
        write(dir.path(), "src/app.ts", "export const x = 1;");

        let ctx = ProjectInstructionsContext::new(dir.path(), 8_000);
        assert!(ctx.context_for("tidy up the codebase").is_none());
        let block = ctx
            .context_for("fix the types in src/app.ts please")
            .expect("the named file puts the scoped rule in play");
        assert!(block.contains("Prefer named exports"));
    }

    #[test]
    fn a_scoped_file_without_apply_to_always_applies() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            ".github/instructions/all.instructions.md",
            "Write tests for every change.",
        );
        let block = ProjectInstructionsContext::new(dir.path(), 8_000)
            .context_for("anything")
            .expect("an unscoped instruction file applies");
        assert!(block.contains("Write tests for every change"));
    }
}
