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

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use localpilot_core::{one_line, SUMMARY_CHARS};
use localpilot_skills::{
    discover_trusted_scoped, user_home, Approval, Confirm, GitFetcher, InstallSpec, Invocation,
    ReadScope, Scope, SkillCatalogStyle, SkillError, SkillSet, SkillsManager, UpdateTarget,
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
    style: &dyn SkillCatalogStyle,
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
        managed => run_managed(managed, cwd, stdin_is_tty, style, out),
    }
}

/// Run a source/install subcommand through the shared [`SkillsManager`], mapping a
/// `SkillError` into printed output plus a non-zero-exit flag.
fn run_managed(
    command: ProjectSkillsCommand,
    cwd: &Path,
    stdin_is_tty: bool,
    style: &dyn SkillCatalogStyle,
    out: &mut dyn Write,
) -> anyhow::Result<SkillsOutcome> {
    let fetcher = GitFetcher;
    let home = user_home();
    let trusted = trust::is_trusted(cwd);
    let now = unix_now_string();
    let manager = SkillsManager::new(cwd, home.as_deref(), trusted, &fetcher, &now);
    let mut confirm = StdinConfirm;

    // A default (effective) read in an untrusted workspace omits project sources;
    // disclose that so an empty or short list is not mistaken for "nothing here."
    // Computed before `command` is consumed by the dispatch match.
    let effective_read = match &command {
        ProjectSkillsCommand::Repo {
            command: SkillsRepoCommand::List { global },
        }
        | ProjectSkillsCommand::Available { global, .. } => Some(*global),
        _ => None,
    };

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
            manager.available_styled(read_scope(global), query.as_deref(), style, out)
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
        ProjectSkillsCommand::Update {
            name,
            all,
            global,
            yes,
        } => match (name, all) {
            (Some(name), false) => manager.update(
                scope(global),
                UpdateTarget::Named(name),
                approval(yes, stdin_is_tty, &mut confirm),
                out,
            ),
            (None, true) => manager.update(
                scope(global),
                UpdateTarget::All,
                approval(yes, stdin_is_tty, &mut confirm),
                out,
            ),
            _ => Err(SkillError::Rejected(
                "name a skill to update, or pass --all".to_string(),
            )),
        },
        ProjectSkillsCommand::Delete { name, global, yes } => manager.delete(
            scope(global),
            &name,
            approval(yes, stdin_is_tty, &mut confirm),
            out,
        ),
        // List/Show are handled in `run` before reaching here; Research runs on the
        // async discovery path (never routed into the synchronous manager).
        ProjectSkillsCommand::List { .. }
        | ProjectSkillsCommand::Show { .. }
        | ProjectSkillsCommand::Research { .. } => Ok(()),
    };

    match result {
        Ok(()) => {
            if let Some(global) = effective_read {
                disclose_untrusted_effective(global, trusted, out)?;
            }
            Ok(SkillsOutcome { had_failure: false })
        }
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

/// Disclose that project-local skills and sources are hidden when the default
/// effective view is read in an untrusted workspace. An explicit `--global` read
/// already selects the global baseline by intent, so it is notice-free. Shared by
/// every default effective read, including `skills research`.
pub(crate) fn disclose_untrusted_effective(
    global: bool,
    trusted: bool,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    if !global && !trusted {
        writeln!(
            out,
            "note: this workspace is not trusted, so project-local skills and sources are hidden. \
             Run `localpilot trust add` to include them, or pass `--global` for the global view."
        )?;
    }
    Ok(())
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
/// their invocation, origin scope, and a one-line summary. The project overlay is
/// loaded only when the workspace is trusted; an untrusted workspace shows the
/// global baseline and a disclosure.
///
/// # Errors
/// Returns an error only if output cannot be written.
pub fn list(root: &Path, global: bool, out: &mut dyn Write) -> anyhow::Result<()> {
    let trusted = trust::is_trusted(root);
    match discover_trusted_scoped(root, trusted, global) {
        Ok(set) => {
            let notes = notes(root, user_home().as_deref(), trusted, global, &set);
            render_list(&set, &notes, out)?;
            disclose_untrusted_effective(global, trusted, out)
        }
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
fn render_list(set: &SkillSet, notes: &Notes, out: &mut dyn Write) -> anyhow::Result<()> {
    // A malformed skill is skipped, not fatal — warn about it but still list the
    // valid ones (LocalHub#38).
    for warning in set.skipped() {
        writeln!(out, "warning: skipped a malformed skill — {warning}")?;
    }
    for line in &notes.extra {
        writeln!(out, "{line}")?;
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
            for note in notes.per_skill.get(name).into_iter().flatten() {
                writeln!(out, "  {note}")?;
            }
        }
    }
    writeln!(out, "\nRead one with: localpilot skills show <name>")?;
    Ok(())
}

/// What `list` and `show` add to the skills they print: a managed copy behind
/// its refreshed source, another definition a skill shadows, and, on their
/// own lines, managed skills whose directory is gone and interrupted changes
/// waiting for recovery. All offline (LocalHub#189).
#[derive(Debug, Default)]
struct Notes {
    per_skill: BTreeMap<String, Vec<String>>,
    extra: Vec<String>,
}

fn notes(root: &Path, home: Option<&Path>, trusted: bool, global: bool, set: &SkillSet) -> Notes {
    let mut notes = Notes::default();
    for s in set.shadowed() {
        notes
            .per_skill
            .entry(s.name.clone())
            .or_default()
            .push(format!(
                "shadows {} [{}]",
                s.loser_dir.display(),
                s.loser_scope.label()
            ));
    }
    let fetcher = GitFetcher;
    let manager = SkillsManager::new(root, home, trusted, &fetcher, "0");
    let read = read_scope(global);
    for m in manager.managed(read).unwrap_or_default() {
        let flag = if m.scope == Scope::Global { " -g" } else { "" };
        if m.missing {
            notes.extra.push(format!(
                "- {} [missing, {} install from {}]: its directory is gone; `localpilot skills \
                 update {}{flag}` reinstalls it, `localpilot skills delete {}{flag}` forgets it",
                m.name,
                scope_label(m.scope),
                short(&m.installed),
                m.name,
                m.name
            ));
        } else if let Some((installed, cached)) = m.stale() {
            notes
                .per_skill
                .entry(m.name.clone())
                .or_default()
                .push(format!(
                    "stale ({} install): {} -> {} (`localpilot skills update {}{flag}`)",
                    scope_label(m.scope),
                    short(installed),
                    short(cached),
                    m.name
                ));
        }
    }
    for scope in manager.recovery_pending(read) {
        notes.extra.push(format!(
            "note: an interrupted skills update or refresh in the {} scope is waiting; the next \
             `localpilot skills` command that changes something recovers it",
            scope_label(scope)
        ));
    }
    notes
}

fn scope_label(scope: Scope) -> &'static str {
    match scope {
        Scope::Project => "project",
        Scope::Global => "global",
    }
}

fn short(commit: &str) -> String {
    commit.chars().take(10).collect()
}

/// Print one skill's body by exact name (a deterministic load). An unknown name is
/// a clean message, never an error.
///
/// # Errors
/// Returns an error only if output cannot be written.
pub fn show(root: &Path, name: &str, global: bool, out: &mut dyn Write) -> anyhow::Result<()> {
    let trusted = trust::is_trusted(root);
    match discover_trusted_scoped(root, trusted, global) {
        Ok(set) => {
            let notes = notes(root, user_home().as_deref(), trusted, global, &set);
            render_show(&set, name, &notes, out)?;
            disclose_untrusted_effective(global, trusted, out)
        }
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
fn render_show(
    set: &SkillSet,
    name: &str,
    notes: &Notes,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
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
            for note in notes
                .per_skill
                .get(&skill.manifest.name)
                .into_iter()
                .flatten()
            {
                writeln!(out, "{note}")?;
            }
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

    #[test]
    fn disclosure_fires_only_for_a_default_untrusted_read() {
        // The one condition that hides the project layer: a default (non-global)
        // read of an untrusted workspace.
        let note = |global: bool, trusted: bool| {
            let mut out = Vec::new();
            disclose_untrusted_effective(global, trusted, &mut out).unwrap();
            String::from_utf8(out).unwrap()
        };
        assert!(note(false, false).contains("project-local skills and sources are hidden"));
        assert!(note(false, true).is_empty(), "a trusted read is silent");
        assert!(
            note(true, false).is_empty(),
            "an explicit --global read is silent"
        );
        assert!(note(true, true).is_empty());
    }

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
        render_list(&resolve(dir.path(), None), &Notes::default(), &mut buf).unwrap();
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
        render_list(
            &resolve(project.path(), Some(home.path())),
            &Notes::default(),
            &mut buf,
        )
        .unwrap();
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
        render_list(&resolve(dir.path(), None), &Notes::default(), &mut buf).unwrap();
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
        render_show(
            &resolve(dir.path(), None),
            "add-provider",
            &Notes::default(),
            &mut hit,
        )
        .unwrap();
        let text = String::from_utf8(hit).unwrap();
        assert!(text.contains("Body of add-provider"), "{text}");
        assert!(
            text.contains("project (.localpilot)"),
            "origin not shown: {text}"
        );

        let mut miss = Vec::new();
        render_show(
            &resolve(dir.path(), None),
            "nope",
            &Notes::default(),
            &mut miss,
        )
        .unwrap();
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
            &Notes::default(),
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

#[cfg(test)]
mod maintenance_tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// A home whose managed `alpha` is behind its refreshed source and
    /// shadows an `.agents` copy, and whose managed `gone` lost its directory.
    fn home_needing_upkeep(home: &Path) {
        let lp = home.join(".localpilot");
        for (root, name) in [
            (lp.join("skills"), "alpha"),
            (home.join(".agents").join("skills"), "alpha"),
        ] {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: d\n---\nbody\n"),
            )
            .unwrap();
        }
        let entry = |name: &str| {
            format!(
                "[[installed]]\nname = \"{name}\"\nsource_id = \"src\"\nsource_url = \"https://example.invalid/r\"\ncommit = \"aaaaaaa1\"\nsource_path = \".localpilot/skills/{name}\"\nscope = \"global\"\ninstalled_at = \"1\"\n"
            )
        };
        std::fs::write(
            lp.join("installed-skills.toml"),
            format!("{}\n{}", entry("alpha"), entry("gone")),
        )
        .unwrap();
        std::fs::write(
            lp.join("skill-sources.toml"),
            "[[source]]\nid = \"src\"\nurl = \"https://example.invalid/r\"\ncommit = \"bbbbbbb2\"\nadded_at = \"1\"\n",
        )
        .unwrap();
    }

    #[test]
    fn list_names_stale_missing_and_shadowed_skills_and_a_pending_recovery() {
        let tmp = tempfile::tempdir().unwrap();
        let (project, home) = (tmp.path().join("project"), tmp.path().join("home"));
        std::fs::create_dir_all(&project).unwrap();
        home_needing_upkeep(&home);
        std::fs::create_dir_all(home.join(".localpilot").join("skills-update")).unwrap();
        std::fs::write(
            home.join(".localpilot")
                .join("skills-update")
                .join("journal.toml"),
            "phase = \"swapping\"\n",
        )
        .unwrap();

        let set = localpilot_skills::discover(&project, Some(&home), false).unwrap();
        let notes = notes(&project, Some(&home), false, false, &set);
        let mut buf = Vec::new();
        render_list(&set, &notes, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains("- alpha [discoverable, global (.localpilot)]"),
            "{text}"
        );
        assert!(text.contains("  stale (global install): aaaaaaa1 -> bbbbbbb2 (`localpilot skills update alpha -g`)"), "{text}");
        assert!(
            text.contains(".agents")
                && text.contains("[global (.agents)]")
                && text.contains("  shadows "),
            "{text}"
        );
        assert!(
            text.contains("- gone [missing, global install from aaaaaaa1]"),
            "{text}"
        );
        assert!(
            text.contains("interrupted skills update or refresh in the global scope"),
            "{text}"
        );

        let mut buf = Vec::new();
        render_show(&set, "alpha", &notes, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains("stale (global install): aaaaaaa1 -> bbbbbbb2"),
            "{text}"
        );
    }

    #[test]
    fn a_project_install_from_a_global_source_is_shown_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let (project, home) = (tmp.path().join("project"), tmp.path().join("home"));
        home_needing_upkeep(&home);
        // The same kind of entry, recorded in the project, whose source is
        // registered globally only.
        let lp = project.join(".localpilot");
        let dir = lp.join("skills").join("beta");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---
name: beta
description: d
---
body
",
        )
        .unwrap();
        std::fs::write(
            lp.join("installed-skills.toml"),
            "[[installed]]
name = \"beta\"
source_id = \"src\"
source_url = \"https://example.invalid/r\"
commit = \"aaaaaaa1\"
source_path = \".localpilot/skills/beta\"
scope = \"project\"
installed_at = \"1\"
",
        )
        .unwrap();
        let set = localpilot_skills::discover(&project, Some(&home), true).unwrap();
        let notes = notes(&project, Some(&home), true, false, &set);
        let beta = notes.per_skill.get("beta").cloned().unwrap_or_default();
        assert!(
            beta.iter().any(|n| n == "stale (project install): aaaaaaa1 -> bbbbbbb2 (`localpilot skills update beta`)"),
            "{beta:?}"
        );
    }

    #[test]
    fn a_clean_home_adds_no_notes() {
        let tmp = tempfile::tempdir().unwrap();
        let set = localpilot_skills::discover(tmp.path(), Some(tmp.path()), false).unwrap();
        let notes = notes(tmp.path(), Some(tmp.path()), false, false, &set);
        assert!(
            notes.extra.is_empty() && notes.per_skill.is_empty(),
            "{notes:?}"
        );
    }
}

#[cfg(test)]
mod update_command_tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use localpilot_skills::PlainSkillCatalogStyle;

    #[test]
    fn update_needs_a_name_or_all() {
        let tmp = tempfile::tempdir().unwrap();
        let mut buf = Vec::new();
        let outcome = run(
            ProjectSkillsCommand::Update {
                name: None,
                all: false,
                global: false,
                yes: true,
            },
            tmp.path(),
            false,
            &PlainSkillCatalogStyle,
            &mut buf,
        )
        .unwrap();
        assert!(outcome.had_failure);
        assert!(String::from_utf8(buf)
            .unwrap()
            .contains("name a skill to update, or pass --all"));
    }
}
