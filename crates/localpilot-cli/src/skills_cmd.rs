//! `localpilot skills`: the deterministic, user-facing skill surface.
//!
//! `list` shows the effective skills — the user-global baseline
//! (`~/.localpilot/skills`, `~/.agents/skills`) overlaid by the project skills,
//! one per name with the project definition winning a collision (LocalHub#39).
//! `show <name>` prints one skill's body by exact name — a deterministic load
//! with no model in the loop. Both are read-only and expose each skill's origin.
//! This is the user side of the skill model (ADR-0027); the model-callable
//! `skill_search`/`skill_load` tools are the pull-based counterpart and are off
//! by default.

use std::io::Write;
use std::path::Path;

use localpilot_core::{one_line, SUMMARY_CHARS};
use localpilot_skills::{
    discover_trusted_scoped, user_home, Approval, Confirm, GitFetcher, InstallSpec, Invocation,
    ReadScope, Scope, SkillError, SkillSet, SkillsManager,
};

use crate::trust;
use crate::{ProjectSkillsCommand, SkillsRepoCommand};

/// Whether a `skills` invocation ended in a user-facing failure, so the process
/// can exit non-zero (mirrors `localpilot models`). A refused or rejected
/// mutation is a failure; a printed read-only result is not.
pub struct SkillsOutcome {
    pub had_failure: bool,
}

/// Execute one `localpilot skills …` subcommand. Read-only commands print and
/// always succeed; a mutation that is refused, rejected, or fails is reported and
/// flagged so the caller exits non-zero. This is the CLI half of the one contract
/// the `/skills` slash surface shares (LocalHub#40).
///
/// # Errors
/// Returns an error only if output cannot be written; user-facing failures are
/// reported in `SkillsOutcome`, not as `Err`.
pub fn run(
    command: ProjectSkillsCommand,
    cwd: &Path,
    stdin_is_tty: bool,
    out: &mut dyn Write,
) -> anyhow::Result<SkillsOutcome> {
    // Read-only listing/reading needs no source manager and never fails the run.
    match command {
        ProjectSkillsCommand::List { global } => {
            list(cwd, global, out)?;
            Ok(SkillsOutcome { had_failure: false })
        }
        ProjectSkillsCommand::Show { name, global } => {
            show(cwd, &name, global, out)?;
            Ok(SkillsOutcome { had_failure: false })
        }
        managed => run_managed(managed, cwd, stdin_is_tty, out),
    }
}

/// Run a source/install subcommand through the shared [`SkillsManager`], mapping a
/// `SkillError` into printed output plus a non-zero-exit flag.
fn run_managed(
    command: ProjectSkillsCommand,
    cwd: &Path,
    stdin_is_tty: bool,
    out: &mut dyn Write,
) -> anyhow::Result<SkillsOutcome> {
    let fetcher = GitFetcher;
    let home = user_home();
    let trusted = trust::is_trusted(cwd);
    let now = unix_now_string();
    let manager = SkillsManager::new(cwd, home.as_deref(), trusted, &fetcher, &now);
    let mut confirm = StdinConfirm;

    let result: Result<(), SkillError> = match command {
        ProjectSkillsCommand::Repo { command } => match command {
            SkillsRepoCommand::Add { url, global, yes } => manager.repo_add(
                scope(global),
                &url,
                approval(yes, stdin_is_tty, &mut confirm),
                out,
            ),
            SkillsRepoCommand::Refresh { url, global, yes } => manager.repo_refresh(
                scope(global),
                url.as_deref(),
                approval(yes, stdin_is_tty, &mut confirm),
                out,
            ),
            SkillsRepoCommand::List { global } => manager.repo_list(read_scope(global), out),
            SkillsRepoCommand::Delete { url, global, yes } => manager.repo_delete(
                scope(global),
                &url,
                approval(yes, stdin_is_tty, &mut confirm),
                out,
            ),
        },
        ProjectSkillsCommand::Available { query, global } => {
            manager.available(read_scope(global), query.as_deref(), out)
        }
        ProjectSkillsCommand::Install {
            name,
            repo,
            all,
            global,
            yes,
        } => match install_spec(name, repo, all) {
            Ok(spec) => manager.install(
                scope(global),
                spec,
                approval(yes, stdin_is_tty, &mut confirm),
                out,
            ),
            Err(err) => Err(err),
        },
        ProjectSkillsCommand::Delete { name, global, yes } => manager.delete(
            scope(global),
            &name,
            approval(yes, stdin_is_tty, &mut confirm),
            out,
        ),
        // List/Show are handled in `run` before reaching here.
        ProjectSkillsCommand::List { .. } | ProjectSkillsCommand::Show { .. } => Ok(()),
    };

    match result {
        Ok(()) => Ok(SkillsOutcome { had_failure: false }),
        Err(err) => {
            writeln!(out, "error: {err}")?;
            Ok(SkillsOutcome { had_failure: true })
        }
    }
}

/// Resolve the mutation scope from the `-g` flag.
fn scope(global: bool) -> Scope {
    if global {
        Scope::Global
    } else {
        Scope::Project
    }
}

/// Resolve the read scope from the `-g` flag: the effective global+project view,
/// or the global scope alone.
fn read_scope(global: bool) -> ReadScope {
    if global {
        ReadScope::GlobalOnly
    } else {
        ReadScope::Effective
    }
}

/// Build the install target from the CLI flags, enforcing the `--all`/`--repo`
/// contract before any effect.
fn install_spec(
    name: Option<String>,
    repo: Option<String>,
    all: bool,
) -> Result<InstallSpec, SkillError> {
    if all {
        if name.is_some() {
            return Err(SkillError::Rejected(
                "--all installs an entire source; do not also name a skill".to_string(),
            ));
        }
        let repo = repo.ok_or_else(|| {
            SkillError::Rejected("--all requires --repo <id> to name the source".to_string())
        })?;
        Ok(InstallSpec::All { repo })
    } else {
        let name = name.ok_or_else(|| {
            SkillError::Rejected(
                "provide a skill name, or use `--all --repo <id>` to install a whole source"
                    .to_string(),
            )
        })?;
        Ok(InstallSpec::Named { name, repo })
    }
}

/// Choose the approval policy: an explicit `--yes`, an interactive terminal, or a
/// non-interactive refusal.
fn approval(yes: bool, stdin_is_tty: bool, confirm: &mut StdinConfirm) -> Approval<'_> {
    if yes {
        Approval::AssumeYes
    } else if stdin_is_tty {
        Approval::Interactive(confirm)
    } else {
        Approval::NonInteractive
    }
}

/// A blocking `[y/N]` prompt on the real terminal, used only when stdin is a TTY
/// and `--yes` was not given.
struct StdinConfirm;

impl Confirm for StdinConfirm {
    fn confirm(&mut self, question: &str) -> bool {
        let mut stdout = std::io::stdout();
        if write!(stdout, "{question} [y/N] ").is_err() {
            return false;
        }
        let _ = stdout.flush();
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).is_err() {
            return false;
        }
        matches!(answer.trim(), "y" | "Y" | "yes")
    }
}

/// The current Unix time in seconds as a string, injected as the manager's clock.
fn unix_now_string() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string()
}

/// List the effective skills (global baseline overlaid by the project) with
/// their invocation, origin scope, and a one-line summary. The user explicitly
/// invoked this, so the project overlay is loaded (the workspace is trusted).
///
/// # Errors
/// Returns an error only if output cannot be written.
pub fn list(root: &Path, global: bool, out: &mut dyn Write) -> anyhow::Result<()> {
    match discover_trusted_scoped(root, true, global) {
        Ok(set) => render_list(&set, out),
        Err(err) => {
            writeln!(out, "could not read skills: {err}")?;
            Ok(())
        }
    }
}

/// Render an effective skill set as a list (the testable core of [`list`]).
///
/// # Errors
/// Returns an error only if output cannot be written.
fn render_list(set: &SkillSet, out: &mut dyn Write) -> anyhow::Result<()> {
    // A malformed skill is skipped, not fatal — warn about it but still list the
    // valid ones (LocalHub#38).
    for warning in set.skipped() {
        writeln!(out, "warning: skipped a malformed skill — {warning}")?;
    }
    let names = set.names();
    if names.is_empty() {
        writeln!(
            out,
            "no skills found (looked under ~/.localpilot/skills, ~/.agents/skills, \
             .localpilot/skills, and .agents/skills)"
        )?;
        return Ok(());
    }
    writeln!(out, "skills:")?;
    for name in names {
        if let Some(skill) = set.by_name(name) {
            let invocation = match skill.manifest.invocation {
                Invocation::UserOnly => "user-only",
                Invocation::Discoverable => "discoverable",
            };
            writeln!(
                out,
                "- {name} [{invocation}, {}]: {}",
                skill.scope.label(),
                one_line(&skill.manifest.description, SUMMARY_CHARS)
            )?;
        }
    }
    writeln!(out, "\nRead one with: localpilot skills show <name>")?;
    Ok(())
}

/// Print one skill's body by exact name (a deterministic load). An unknown name is
/// a clean message, never an error.
///
/// # Errors
/// Returns an error only if output cannot be written.
pub fn show(root: &Path, name: &str, global: bool, out: &mut dyn Write) -> anyhow::Result<()> {
    match discover_trusted_scoped(root, true, global) {
        Ok(set) => render_show(&set, name, out),
        Err(err) => {
            writeln!(out, "could not read skills: {err}")?;
            Ok(())
        }
    }
}

/// Render one effective skill's body by name (the testable core of [`show`]).
///
/// # Errors
/// Returns an error only if output cannot be written.
fn render_show(set: &SkillSet, name: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    for warning in set.skipped() {
        writeln!(out, "warning: skipped a malformed skill — {warning}")?;
    }
    match set.by_name(name.trim()) {
        Some(skill) => {
            writeln!(
                out,
                "# skill: {} [{}]",
                skill.manifest.name,
                skill.scope.label()
            )?;
            if let Some(hint) = &skill.manifest.argument_hint {
                writeln!(out, "argument: {hint}")?;
            }
            if !skill.manifest.required_tools.is_empty() {
                writeln!(
                    out,
                    "declares required tools: {}",
                    skill.manifest.required_tools.join(", ")
                )?;
            }
            if !skill.manifest.permissions.is_empty() {
                writeln!(
                    out,
                    "declares permissions: {} (not granted; any action still goes through the \
                     permission gate)",
                    skill.manifest.permissions.join(", ")
                )?;
            }
            writeln!(out, "\n{}", skill.instructions.trim_end())?;
        }
        None => writeln!(out, "no skill named \"{}\"", name.trim())?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// Write a `SKILL.md`-only skill under `<root>/<sub>/<name>`, where `sub` is
    /// e.g. `.localpilot/skills` or `.agents/skills`. `root` is a project root or
    /// an injected home directory.
    fn write_skill_md_in(root: &Path, sub: &str, name: &str, description: &str, user_only: bool) {
        let mut dir = root.to_path_buf();
        for part in sub.split('/') {
            dir.push(part);
        }
        dir.push(name);
        std::fs::create_dir_all(&dir).unwrap();
        let flag = if user_only {
            "disable-model-invocation: true\n"
        } else {
            ""
        };
        std::fs::write(
            dir.join("SKILL.md"),
            format!(
                "---\nname: {name}\ndescription: {description}\n{flag}---\n\nBody of {name}.\n"
            ),
        )
        .unwrap();
    }

    /// A project `.localpilot/skills` skill.
    fn write_skill_md(root: &Path, name: &str, description: &str, user_only: bool) {
        write_skill_md_in(root, ".localpilot/skills", name, description, user_only);
    }

    /// The effective set for a project, with the global baseline injected from
    /// `home` (or `None`), so tests never touch the host's real home.
    fn resolve(root: &Path, home: Option<&Path>) -> SkillSet {
        localpilot_skills::discover(root, home, true).unwrap()
    }

    #[test]
    fn list_shows_invocation_origin_and_summary() {
        let dir = tempfile::tempdir().unwrap();
        write_skill_md(dir.path(), "add-provider", "guide adding a provider", false);
        write_skill_md(dir.path(), "secret-step", "by hand only", true);
        let mut buf = Vec::new();
        render_list(&resolve(dir.path(), None), &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains("add-provider [discoverable, project (.localpilot)]"),
            "{text}"
        );
        assert!(
            text.contains("secret-step [user-only, project (.localpilot)]"),
            "{text}"
        );
    }

    #[test]
    fn list_includes_global_skills_and_marks_project_overrides() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        // A global-only skill, and a name defined in both scopes.
        write_skill_md_in(
            home.path(),
            ".agents/skills",
            "threejs-webgl",
            "global three.js",
            false,
        );
        write_skill_md_in(
            home.path(),
            ".agents/skills",
            "modern-web-design",
            "global design",
            false,
        );
        write_skill_md(project.path(), "modern-web-design", "project design", false);

        let mut buf = Vec::new();
        render_list(&resolve(project.path(), Some(home.path())), &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        // Global-only skill shows its global origin…
        assert!(
            text.contains("threejs-webgl [discoverable, global (.agents)]"),
            "{text}"
        );
        // …and the overridden name appears once, as the project definition.
        assert!(
            text.contains("modern-web-design [discoverable, project (.localpilot)]"),
            "{text}"
        );
        assert_eq!(
            text.matches("modern-web-design").count(),
            1,
            "duplicate name in listing: {text}"
        );
    }

    #[test]
    fn list_summary_is_capped_to_one_line_with_ellipsis() {
        // Equivalence guard for the move to localpilot_core::one_line: a long skill
        // description is collapsed to one capped line + ellipsis in the user listing.
        let dir = tempfile::tempdir().unwrap();
        let long = format!("guide adding {}", "a provider integration ".repeat(20));
        write_skill_md(dir.path(), "add-provider", long.trim(), false);
        let mut buf = Vec::new();
        render_list(&resolve(dir.path(), None), &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        let line = text
            .lines()
            .find(|l| l.contains("add-provider"))
            .expect("listing line");
        assert!(line.contains('…'), "summary not ellipsized: {line:?}");
        assert!(
            line.chars().count() < long.chars().count(),
            "summary not truncated: {line:?}"
        );
    }

    #[test]
    fn show_prints_a_body_with_origin_and_a_clean_miss_otherwise() {
        let dir = tempfile::tempdir().unwrap();
        write_skill_md(dir.path(), "add-provider", "guide adding a provider", false);

        let mut hit = Vec::new();
        render_show(&resolve(dir.path(), None), "add-provider", &mut hit).unwrap();
        let text = String::from_utf8(hit).unwrap();
        assert!(text.contains("Body of add-provider"), "{text}");
        assert!(
            text.contains("project (.localpilot)"),
            "origin not shown: {text}"
        );

        let mut miss = Vec::new();
        render_show(&resolve(dir.path(), None), "nope", &mut miss).unwrap();
        assert!(String::from_utf8(miss).unwrap().contains("no skill named"));
    }

    #[test]
    fn show_reaches_a_global_skill_from_an_unrelated_project() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_skill_md_in(
            home.path(),
            ".localpilot/skills",
            "threejs-webgl",
            "global",
            false,
        );

        let mut buf = Vec::new();
        render_show(
            &resolve(project.path(), Some(home.path())),
            "threejs-webgl",
            &mut buf,
        )
        .unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("Body of threejs-webgl"), "{text}");
        assert!(
            text.contains("global (.localpilot)"),
            "origin not shown: {text}"
        );
    }
}
