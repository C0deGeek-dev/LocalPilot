//! `unshackled init` and `unshackled harness status`.
//!
//! Status is read-only and must work without a model provider, so it never
//! constructs the provider registry or touches the network.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use unshackled_config::{CliOverrides, Config, ConfigPaths};
use unshackled_harness::{
    resume_one_step, run_intake, run_plan, Brief, Progress, RuleEngine, SessionConfig,
    SessionRuntime, QUOTA_PAUSE_KEY,
};
use unshackled_llm::{ModelProvider, ProviderRegistry};
use unshackled_quota::{decide_resume, PausedRun, ResumeContext, ResumeDecision, ResumePolicy};
use unshackled_recovery::{RecoveryBudget, RecoveryEngine};
use unshackled_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use unshackled_store::Store;
use unshackled_tools::ToolRegistry;

const DEFAULT_CONFIG: &str = "[harness]\n\
mode = \"agent\"\n\
attempts_per_step = 3\n\
auto_commit = true\n\
# test_command = \"cargo test\"\n\n\
[permissions]\n\
profile = \"default\"\n\n\
[provider]\n\
default = \"local\"\n";

const GITIGNORE_ENTRY: &str = ".unshackled/";

/// Initialize project-local harness state.
///
/// # Errors
/// Returns an error if files cannot be written or git cannot be initialized.
pub fn init(root: &Path, init_git: bool) -> anyhow::Result<String> {
    let mut created = Vec::new();

    let config_path = root.join(".unshackled.toml");
    if config_path.exists() {
        created.push(".unshackled.toml (already present, left unchanged)".to_string());
    } else {
        std::fs::write(&config_path, DEFAULT_CONFIG)?;
        created.push(".unshackled.toml".to_string());
    }

    ensure_gitignore_entry(root, &mut created)?;

    if init_git && !root.join(".git").exists() {
        let status = std::process::Command::new("git")
            .arg("init")
            .current_dir(root)
            .status()?;
        if status.success() {
            created.push("git repository".to_string());
        }
    }

    Ok(format!("initialized: {}", created.join(", ")))
}

fn ensure_gitignore_entry(root: &Path, created: &mut Vec<String>) -> anyhow::Result<()> {
    let gitignore = root.join(".gitignore");
    let existing = std::fs::read_to_string(&gitignore).unwrap_or_default();
    if existing.lines().any(|line| line.trim() == GITIGNORE_ENTRY) {
        return Ok(());
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(GITIGNORE_ENTRY);
    updated.push('\n');
    std::fs::write(&gitignore, updated)?;
    created.push(".gitignore entry for .unshackled/".to_string());
    Ok(())
}

/// A read-only harness status snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    pub branch: Option<String>,
    pub next_step: Option<String>,
    pub completed: usize,
    pub total: usize,
    pub dirty: bool,
    pub test_command: Option<String>,
    pub default_provider: String,
    pub provider_credential_present: bool,
}

impl StatusReport {
    /// Render the status as deterministic text.
    #[must_use]
    pub fn render(&self) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        let _ = writeln!(s, "branch: {}", self.branch.as_deref().unwrap_or("(none)"));
        let _ = writeln!(
            s,
            "progress: {}/{} steps complete",
            self.completed, self.total
        );
        let _ = writeln!(
            s,
            "next step: {}",
            self.next_step.as_deref().unwrap_or("(none)")
        );
        let _ = writeln!(
            s,
            "working tree: {}",
            if self.dirty { "dirty" } else { "clean" }
        );
        let _ = writeln!(
            s,
            "test command: {}",
            self.test_command.as_deref().unwrap_or("(unset)")
        );
        let credential = if self.provider_credential_present {
            "set"
        } else {
            "not set"
        };
        let _ = writeln!(
            s,
            "provider: {} (credential {credential})",
            self.default_provider
        );
        s
    }
}

/// Gather harness status from the working directory.
///
/// # Errors
/// Returns an error if the current directory or configuration cannot be read.
pub fn gather_status(root: &Path) -> anyhow::Result<StatusReport> {
    let config = unshackled_config::load(&ConfigPaths::standard(root), &CliOverrides::default())
        .unwrap_or_else(|_| Config::default());

    let progress = std::fs::read_to_string(root.join("PROGRESS.md"))
        .ok()
        .and_then(|text| Progress::parse(&text).ok());
    let (next_step, completed, total) = match &progress {
        Some(p) => (
            p.next_incomplete()
                .map(|s| format!("{}. {}", s.number, s.description)),
            p.completed_count(),
            p.steps.len(),
        ),
        None => (None, 0, 0),
    };

    let default_provider = config.provider.default.clone();
    let provider_credential_present = config.resolve_credential(&default_provider).is_some();

    Ok(StatusReport {
        branch: git_line(root, &["rev-parse", "--abbrev-ref", "HEAD"]),
        next_step,
        completed,
        total,
        dirty: git_line(root, &["status", "--porcelain"]).is_some_and(|s| !s.trim().is_empty()),
        test_command: config.harness.test_command.clone(),
        default_provider,
        provider_credential_present,
    })
}

fn git_line(root: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

/// Print the harness status to `out`.
///
/// # Errors
/// Returns an error from gathering status or writing output.
pub fn status(root: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let report = gather_status(root)?;
    out.write_all(report.render().as_bytes())?;
    Ok(())
}

fn provider_for(
    root: &Path,
    provider_id: Option<&str>,
) -> anyhow::Result<std::sync::Arc<dyn unshackled_llm::ModelProvider>> {
    let config = unshackled_config::load(&ConfigPaths::standard(root), &CliOverrides::default())?;
    let registry = ProviderRegistry::from_config(&config)?;
    match provider_id {
        Some(id) => registry.get(id),
        None => registry.default_provider(),
    }
    .cloned()
    .ok_or_else(|| anyhow::anyhow!("no provider is configured"))
}

/// Run intake: an idea becomes `brief.md`, with an `.unshackled/intake.jsonl`
/// record.
///
/// # Errors
/// Returns an error if the provider fails or files cannot be written.
pub async fn intake(
    root: &Path,
    model: &str,
    provider_id: Option<&str>,
    idea: &str,
) -> anyhow::Result<()> {
    let provider = provider_for(root, provider_id)?;
    let brief = run_intake(provider.as_ref(), model, idea).await?;
    std::fs::write(root.join("brief.md"), brief.render())?;

    let intake_dir = root.join(".unshackled");
    std::fs::create_dir_all(&intake_dir)?;
    let record = serde_json::json!({ "idea": idea, "name": brief.name });
    let mut line = serde_json::to_string(&record)?;
    line.push('\n');
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(intake_dir.join("intake.jsonl"))?;
    file.write_all(line.as_bytes())?;
    Ok(())
}

/// Run planning: `brief.md` becomes `PROGRESS.md`.
///
/// # Errors
/// Returns an error if the brief is missing/invalid or the provider fails.
pub async fn plan(root: &Path, model: &str, provider_id: Option<&str>) -> anyhow::Result<()> {
    let brief_text = std::fs::read_to_string(root.join("brief.md")).map_err(|_| {
        anyhow::anyhow!("brief.md not found; run `unshackled harness intake` first")
    })?;
    let brief = Brief::parse(&brief_text)?;
    let provider = provider_for(root, provider_id)?;
    let summary = repo_summary(root);
    let progress = run_plan(provider.as_ref(), model, &brief, &summary).await?;
    std::fs::write(root.join("PROGRESS.md"), progress.render())?;
    Ok(())
}

/// Add a feature to an existing brief and plan, without renumbering completed
/// steps. This is deterministic and needs no provider.
///
/// # Errors
/// Returns an error if the brief or progress files are missing or invalid.
pub fn feature(root: &Path, description: &str) -> anyhow::Result<()> {
    let mut brief = Brief::parse(&std::fs::read_to_string(root.join("brief.md"))?)?;
    brief.add_requirement(description);
    std::fs::write(root.join("brief.md"), brief.render())?;

    let mut progress = Progress::parse(&std::fs::read_to_string(root.join("PROGRESS.md"))?)?;
    progress.append_step(format!("Implement: {description}"));
    std::fs::write(root.join("PROGRESS.md"), progress.render())?;
    Ok(())
}

/// Run harness steps from `PROGRESS.md` until none remain, a step is blocked, or
/// the step cap is reached. Each step runs with fresh context.
///
/// # Errors
/// Returns an error if config/provider setup or a step fails.
pub async fn resume(
    root: &Path,
    model: &str,
    provider_id: Option<&str>,
    profile: Profile,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let config = unshackled_config::load(&ConfigPaths::standard(root), &CliOverrides::default())
        .unwrap_or_else(|_| Config::default());
    let provider = provider_for(root, provider_id)?;
    let workspace = Workspace::new(root)?;
    let rules = RuleEngine::with_baseline(&config.harness.rules);
    let test_command = config.harness.test_command.clone();
    let max_attempts = config.harness.attempts_per_step;

    const MAX_STEPS: usize = 100;
    for _ in 0..MAX_STEPS {
        let remaining = std::fs::read_to_string(root.join("PROGRESS.md"))
            .ok()
            .and_then(|t| Progress::parse(&t).ok())
            .map(|p| p.next_incomplete().is_some())
            .unwrap_or(false);
        if !remaining {
            writeln!(out, "all steps complete")?;
            break;
        }

        let mut runtime = build_runtime(
            root,
            Arc::clone(&provider),
            workspace.clone(),
            profile,
            model,
        );
        let outcome = resume_one_step(
            &mut runtime,
            root,
            &rules,
            test_command.as_deref(),
            max_attempts,
        )
        .await?;
        if outcome.committed {
            writeln!(out, "step {} complete", outcome.step_number)?;
        } else {
            writeln!(
                out,
                "step {} blocked: {}",
                outcome.step_number,
                outcome.blocked_reason.as_deref().unwrap_or("unknown")
            )?;
            break;
        }
    }
    Ok(())
}

/// Continue a run that paused on a provider quota/rate limit, if it is now safe.
///
/// # Errors
/// Returns an error if the paused-run file is unreadable or resume fails.
pub async fn wait_resume(
    root: &Path,
    model: &str,
    provider_id: Option<&str>,
    profile: Profile,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let store = Store::open(root);
    let Some(bytes) = store.get_cache(QUOTA_PAUSE_KEY)? else {
        writeln!(out, "no paused run")?;
        return Ok(());
    };
    let paused: PausedRun =
        serde_json::from_slice(&bytes).map_err(|e| anyhow::anyhow!("invalid paused-run file: {e}"))?;

    let config = unshackled_config::load(&ConfigPaths::standard(root), &CliOverrides::default())
        .unwrap_or_else(|_| Config::default());
    let policy = ResumePolicy::from(&config.quota);
    let now = now_unix();
    let ctx = ResumeContext {
        window_elapsed: paused.resume_eligible_unix.is_none_or(|t| now >= t),
        at_step_boundary: true,
        workspace_clean: !workspace_dirty(root),
        pending_destructive_approval: false,
        user_cancelled: false,
        provider_identity_changed: false,
        waited: Duration::from_secs(now.saturating_sub(paused.paused_at_unix)),
    };

    match decide_resume(&policy, &ctx) {
        ResumeDecision::Resume => {
            writeln!(out, "resuming paused run at step {}", paused.step_number)?;
            store.delete_cache(QUOTA_PAUSE_KEY)?;
            resume(root, model, provider_id, profile, out).await?;
        }
        ResumeDecision::Wait => {
            let eta = paused
                .resume_eligible_unix
                .map_or(0, |t| t.saturating_sub(now));
            writeln!(out, "paused ({}); resume eligible in ~{eta}s", paused.reason)?;
        }
        ResumeDecision::AskUser => {
            writeln!(
                out,
                "auto_resume is 'ask'; set quota.auto_resume = run|global to continue automatically"
            )?;
        }
        ResumeDecision::BlockedBy(reason) => {
            writeln!(out, "cannot resume: {reason}")?;
        }
    }
    Ok(())
}

fn workspace_dirty(root: &Path) -> bool {
    git_line(root, &["status", "--porcelain"]).is_some_and(|s| !s.trim().is_empty())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn build_runtime(
    root: &Path,
    provider: Arc<dyn ModelProvider>,
    workspace: Workspace,
    profile: Profile,
    model: &str,
) -> SessionRuntime {
    SessionRuntime::new(
        provider,
        ToolRegistry::with_builtins(),
        PermissionEngine::new(profile, Vec::new()),
        Box::new(ScriptedApprover::new(Vec::new())),
        Store::open(root),
        workspace,
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            model: model.to_string(),
            interactivity: Interactivity::NonInteractive,
            trusted: true,
            ..SessionConfig::default()
        },
        Vec::new(),
    )
}

fn repo_summary(root: &Path) -> String {
    let mut entries: Vec<String> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .collect();
    entries.sort();
    format!("Top-level entries: {}", entries.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_render_is_stable() {
        let report = StatusReport {
            branch: Some("feature/parser-errors".to_string()),
            next_step: Some("2. Implement parser errors".to_string()),
            completed: 1,
            total: 3,
            dirty: false,
            test_command: Some("cargo test".to_string()),
            default_provider: "local".to_string(),
            provider_credential_present: false,
        };
        insta::assert_snapshot!(report.render());
    }
}
