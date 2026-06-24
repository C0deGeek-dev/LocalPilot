//! `localpilot self-review propose-issue` / `propose-pr` / `drafts` / `emit-draft`
//! — the outward half of the human-gated self-improvement loop (ADR-0053).
//!
//! The agent may author a **draft** issue or PR describing a ranked self-review
//! finding and write it, redacted, to the project-local `.localpilot/outward/`
//! store for human inspection (`propose-issue`/`propose-pr`) — touching nothing
//! outside the machine. A human inspects drafts (`drafts list`/`show`/`discard`)
//! and, only with an explicit `--approve`, publishes one as a **draft** issue/PR
//! via the official `gh` CLI (`emit-draft`). The gate is the same value-typed
//! `ApprovalToken` that promotes a patch: it is minted only on the `--approve`
//! path, so the autonomous loop can propose but never publish. Publication is
//! draft-only, dry-run by default, restricted to an allowlisted target, and never
//! marks ready / merges.

use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context};
use clap::Subcommand;
use localpilot_config::{CliOverrides, ConfigPaths, SelfImprovementConfig};
use localpilot_patchgen::{
    discard_outward_draft, list_outward_drafts, record_outward_event, ApprovalToken,
    ChangeProvenance, DraftRequest, EmitPhase, OutwardDraft, OutwardEvent, OutwardKind,
    OutwardPolicy, PublishPlan,
};
use localpilot_selfreview::{draft_spec_for_finding, review, Finding, ReviewOptions};

/// `self-review drafts` subcommands: inspect or clean locally proposed drafts.
#[derive(Debug, Subcommand)]
pub enum OutwardDraftsCommand {
    /// List the ids of every locally proposed outward draft.
    List,
    /// Show one draft's full contents and the (dry-run) publish plan.
    Show {
        /// The draft id.
        #[arg(long)]
        id: String,
    },
    /// Discard a locally proposed draft (remove it from the store).
    Discard {
        /// The draft id.
        #[arg(long)]
        id: String,
    },
}

/// Load the outward-emit policy from the project config. The policy is plain data
/// (`enabled` + the allowlist), both default-off, so a project with no
/// `[self_improvement]` block yields an inert policy that publishes nothing.
fn load_policy(repo_root: &Path) -> anyhow::Result<OutwardPolicy> {
    let config =
        localpilot_config::load(&ConfigPaths::standard(repo_root), &CliOverrides::default())?;
    Ok(policy_from(&config.self_improvement))
}

/// Project a `[self_improvement]` config block into the gated crate's plain policy.
fn policy_from(config: &SelfImprovementConfig) -> OutwardPolicy {
    OutwardPolicy::new(config.enabled, config.outward_targets.clone())
}

/// A collision-resistant draft id encoding its kind and the finding rank.
fn outward_id(kind: OutwardKind, finding_rank: usize) -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let tag = match kind {
        OutwardKind::Issue => "issue",
        OutwardKind::DraftPr => "pr",
    };
    format!("outward-{tag}-{finding_rank}-{secs}")
}

/// Build a gated [`OutwardDraft`] from a ranked finding. The policy is applied by
/// [`OutwardDraft::new`] (refuses a disabled feature or an un-allowlisted target
/// before composing anything), and the body is redacted there. No IO, no network.
fn draft_from_finding(
    kind: OutwardKind,
    finding_rank: usize,
    finding: &Finding,
    target: &str,
    head_branch: Option<String>,
    policy: &OutwardPolicy,
) -> anyhow::Result<OutwardDraft> {
    let spec = draft_spec_for_finding(finding)
        .ok_or_else(|| anyhow!("finding {finding_rank} carries no evidence to describe"))?;
    // Provenance: the source finding, the producing surface, the rationale, and
    // the risk note, rendered into the (redacted) draft body.
    let mut provenance =
        ChangeProvenance::new(spec.source, "localpilot self-review", spec.rationale);
    provenance.risks = spec.risks;
    let request = DraftRequest {
        id: outward_id(kind, finding_rank),
        kind,
        target_repo: target.to_string(),
        title: spec.title,
        description: spec.description,
        head_branch,
    };
    OutwardDraft::new(request, &provenance, policy).map_err(anyhow::Error::from)
}

/// Resolve a ranked finding by 1-based rank from a read-only self-review of
/// `repo_root`. Mirrors `propose-patch`: no prior-lesson fetch, so it stays offline
/// and never initialises a store.
fn finding_at_rank(repo_root: &Path, finding_rank: usize) -> anyhow::Result<Finding> {
    if finding_rank == 0 {
        return Err(anyhow!(
            "--finding is 1-based; use the rank shown by `localpilot self-review`"
        ));
    }
    let report = review(repo_root, &ReviewOptions::default());
    report
        .findings
        .get(finding_rank - 1)
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "no finding ranked {finding_rank}; `localpilot self-review` lists {} finding(s)",
                report.findings.len()
            )
        })
}

/// `self-review propose-issue`: author a draft issue for a finding and persist it.
pub fn run_propose_issue(
    repo_root: &Path,
    finding_rank: usize,
    target: &str,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    run_propose(
        repo_root,
        OutwardKind::Issue,
        finding_rank,
        target,
        None,
        out,
    )
}

/// `self-review propose-pr`: author a draft PR for a finding and persist it.
pub fn run_propose_pr(
    repo_root: &Path,
    finding_rank: usize,
    target: &str,
    head: &str,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    run_propose(
        repo_root,
        OutwardKind::DraftPr,
        finding_rank,
        target,
        Some(head.to_string()),
        out,
    )
}

fn run_propose(
    repo_root: &Path,
    kind: OutwardKind,
    finding_rank: usize,
    target: &str,
    head_branch: Option<String>,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let policy = load_policy(repo_root)?;
    let finding = finding_at_rank(repo_root, finding_rank)?;
    let draft = draft_from_finding(kind, finding_rank, &finding, target, head_branch, &policy)
        .context("authoring the outward draft")?;
    draft
        .persist(repo_root)
        .context("writing the draft to the outward store")?;
    record_outward_event(
        repo_root,
        &OutwardEvent {
            id: draft.id.clone(),
            phase: EmitPhase::Proposed,
            target_repo: draft.target_repo.clone(),
            url: None,
            reviewer: None,
        },
    )
    .context("recording the proposed-draft event")?;

    let kind_label = match kind {
        OutwardKind::Issue => "issue",
        OutwardKind::DraftPr => "draft PR",
    };
    writeln!(
        out,
        "Proposed {kind_label} draft `{}` for `{}` — inspect before emitting:",
        draft.id, draft.target_repo
    )?;
    writeln!(out, "  title: {}", draft.title)?;
    writeln!(out, "  store: .localpilot/outward/{}.json", draft.id)?;
    writeln!(out)?;
    writeln!(
        out,
        "Inspect:\n  localpilot self-review drafts show --id {}",
        draft.id
    )?;
    writeln!(
        out,
        "Publish as a draft (human approval required):\n  localpilot self-review emit-draft --id {} --reviewer <you> --approve",
        draft.id
    )?;
    writeln!(
        out,
        "Discard:\n  localpilot self-review drafts discard --id {}",
        draft.id
    )?;
    Ok(())
}

/// `self-review drafts` dispatch.
pub fn run_drafts(
    repo_root: &Path,
    command: OutwardDraftsCommand,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    match command {
        OutwardDraftsCommand::List => run_drafts_list(repo_root, out),
        OutwardDraftsCommand::Show { id } => run_drafts_show(repo_root, &id, out),
        OutwardDraftsCommand::Discard { id } => run_drafts_discard(repo_root, &id, out),
    }
}

fn run_drafts_list(repo_root: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let ids = list_outward_drafts(repo_root).context("listing outward drafts")?;
    if ids.is_empty() {
        writeln!(out, "no outward drafts proposed")?;
        return Ok(());
    }
    writeln!(out, "{} outward draft(s):", ids.len())?;
    for id in ids {
        match OutwardDraft::load(repo_root, &id) {
            Ok(draft) => {
                let kind = match draft.kind {
                    OutwardKind::Issue => "issue",
                    OutwardKind::DraftPr => "draft-pr",
                };
                writeln!(
                    out,
                    "- {id} [{kind} → {}] {}",
                    draft.target_repo, draft.title
                )?;
            }
            Err(_) => writeln!(out, "- {id} (unreadable)")?,
        }
    }
    Ok(())
}

fn run_drafts_show(repo_root: &Path, id: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    let draft = OutwardDraft::load(repo_root, id).context("loading the draft")?;
    let kind = match draft.kind {
        OutwardKind::Issue => "issue",
        OutwardKind::DraftPr => "draft PR",
    };
    writeln!(out, "draft {id} [{kind}] → {}", draft.target_repo)?;
    writeln!(out, "title: {}", draft.title)?;
    if let Some(head) = &draft.head_branch {
        writeln!(out, "head:  {head}")?;
    }
    writeln!(out, "body:")?;
    writeln!(out, "{}", draft.body)?;
    writeln!(out)?;
    writeln!(out, "publish plan (dry run — nothing is run):")?;
    writeln!(out, "  {}", render_argv(&draft.publish_argv()))?;
    Ok(())
}

fn run_drafts_discard(repo_root: &Path, id: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    discard_outward_draft(repo_root, id).context("discarding the draft")?;
    writeln!(out, "Discarded outward draft `{id}`.")?;
    Ok(())
}

/// `self-review emit-draft`: the human-approved publish step. Dry-run by default;
/// `--approve` mints the [`ApprovalToken`] (the only place it is minted), preflights
/// `gh`, and publishes the draft via `gh issue create` / `gh pr create --draft`.
pub fn run_emit_draft(
    repo_root: &Path,
    id: &str,
    reviewer: &str,
    approve: bool,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let policy = load_policy(repo_root)?;
    let draft = OutwardDraft::load(repo_root, id).context("loading the draft to emit")?;

    // Re-check the allowlist at emit time: the target must still be enabled and
    // allowlisted, even though it was at propose time (config may have changed).
    if !policy.allows(&draft.target_repo) {
        return Err(anyhow!(
            "target `{}` is not enabled/allowlisted in [self_improvement]; refusing to emit",
            draft.target_repo
        ));
    }

    // The structural gate: no token is minted and nothing is published without an
    // explicit human act. Without `--approve`, print the plan and stop (dry run).
    if !approve {
        writeln!(out, "DRY RUN — nothing is published.")?;
        writeln!(
            out,
            "would publish draft `{id}` to `{}`:",
            draft.target_repo
        )?;
        writeln!(out, "  gh {}", render_argv(&draft.publish_argv()))?;
        writeln!(out)?;
        writeln!(
            out,
            "To publish this as a draft, re-run with: --approve --reviewer <you>"
        )?;
        return Ok(());
    }
    if reviewer.trim().is_empty() {
        return Err(anyhow!(
            "--reviewer must name the human approving this publish"
        ));
    }

    // Preflight: gh must be installed and authenticated; surface the account.
    let account = preflight_gh().context("gh preflight")?;
    writeln!(out, "gh account: {account}")?;

    // Mint the approval token — the single mint path, mirroring `promote`. The
    // autonomous loop never reaches here (it never passes `--approve`).
    let token = ApprovalToken::approve(draft.id.clone(), reviewer);
    let plan = draft
        .publish_plan(&token)
        .context("building the gated publish plan")?;
    record_outward_event(
        repo_root,
        &OutwardEvent {
            id: draft.id.clone(),
            phase: EmitPhase::Approved,
            target_repo: draft.target_repo.clone(),
            url: None,
            reviewer: Some(reviewer.to_string()),
        },
    )
    .context("recording the approved event")?;

    let url = publish(&plan).context("publishing the draft via gh")?;
    record_outward_event(
        repo_root,
        &OutwardEvent {
            id: draft.id.clone(),
            phase: EmitPhase::Published,
            target_repo: draft.target_repo.clone(),
            url: Some(url.clone()),
            reviewer: Some(reviewer.to_string()),
        },
    )
    .context("recording the published event")?;

    writeln!(
        out,
        "Published draft `{id}` to `{}` (reviewer: {reviewer}).\n  {url}",
        draft.target_repo
    )?;
    Ok(())
}

/// Preflight `gh`: confirm it is installed and authenticated, returning a short
/// account description for the approval output. Refuses clearly otherwise — a
/// failed/mis-attributed publish is worse than no publish.
fn preflight_gh() -> anyhow::Result<String> {
    let version = Command::new("gh").arg("--version").output();
    match version {
        Ok(o) if o.status.success() => {}
        Ok(_) => return Err(anyhow!("`gh` is installed but did not report a version")),
        Err(_) => {
            return Err(anyhow!(
                "the GitHub CLI `gh` was not found on PATH; install it to publish drafts"
            ))
        }
    }
    let auth = Command::new("gh")
        .args(["auth", "status"])
        .output()
        .map_err(|e| anyhow!("could not run `gh auth status`: {e}"))?;
    if !auth.status.success() {
        return Err(anyhow!(
            "`gh` is not authenticated; run `gh auth login` first"
        ));
    }
    // `gh auth status` prints the account to stderr. Surface its first line so the
    // human sees which account the draft would be attributed to.
    let text = String::from_utf8_lossy(&auth.stderr);
    let account = text
        .lines()
        .map(str::trim)
        .find(|l| l.contains("account") || l.contains("Logged in"))
        .unwrap_or("authenticated")
        .to_string();
    Ok(account)
}

/// Execute a gated [`PublishPlan`] via `gh`, returning the created issue/PR URL.
/// Arguments are passed as an argv array (no shell), so the redacted title/body can
/// never become another command.
fn publish(plan: &PublishPlan) -> anyhow::Result<String> {
    let output = Command::new(&plan.program)
        .args(&plan.args)
        .output()
        .map_err(|e| anyhow!("could not run `{}`: {e}", plan.program))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "`gh` failed (exit {}): {}",
            output.status.code().unwrap_or(-1),
            stderr.trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    // `gh issue/pr create` prints the created URL on stdout.
    let url = stdout
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("http"))
        .unwrap_or_else(|| stdout.trim())
        .to_string();
    Ok(url)
}

/// Render an argv for human display (the dry-run plan), quoting any element that
/// contains whitespace so a multi-line body reads as one argument.
fn render_argv(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.chars().any(char::is_whitespace) {
                format!("{:?}", a)
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use localpilot_selfreview::{FindingKind, Severity};

    fn policy() -> OutwardPolicy {
        OutwardPolicy::new(true, vec!["owner/repo".to_string()])
    }

    fn todo_finding() -> Finding {
        Finding::new(
            FindingKind::Todo,
            Severity::Low,
            0.9,
            "stale TODO marker left in a tracked file".to_string(),
        )
        .at_path("src/a.rs")
    }

    #[test]
    fn proposing_writes_a_draft_to_the_isolated_store() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let draft = draft_from_finding(
            OutwardKind::Issue,
            1,
            &todo_finding(),
            "owner/repo",
            None,
            &policy(),
        )
        .unwrap();
        draft.persist(root).unwrap();
        // The draft lives under the git-ignored .localpilot/ tree, nowhere else.
        let stored = root.join(format!(".localpilot/outward/{}.json", draft.id));
        assert!(stored.exists());
        let reloaded = OutwardDraft::load(root, &draft.id).unwrap();
        assert_eq!(reloaded.target_repo, "owner/repo");
        assert!(reloaded.body.contains("stale TODO marker"));
    }

    #[test]
    fn an_unallowlisted_target_is_refused_at_propose() {
        let err = draft_from_finding(
            OutwardKind::Issue,
            1,
            &todo_finding(),
            "owner/not-allowed",
            None,
            &policy(),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("not on the") || err.to_string().contains("allowlist"),
            "{err}"
        );
    }

    #[test]
    fn a_disabled_feature_refuses_to_propose() {
        let off = OutwardPolicy::new(false, vec!["owner/repo".to_string()]);
        let err = draft_from_finding(
            OutwardKind::Issue,
            1,
            &todo_finding(),
            "owner/repo",
            None,
            &off,
        )
        .unwrap_err();
        assert!(err.to_string().contains("disabled"), "{err}");
    }

    #[test]
    fn a_secret_in_the_finding_evidence_is_redacted_in_the_draft() {
        // Even though findings rarely carry secrets, the CLI→patchgen wiring must
        // redact: a secret-shaped evidence string never reaches the stored body.
        let finding = Finding::new(
            FindingKind::Friction,
            Severity::Medium,
            0.8,
            "leaked key ghp_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789 in the audit".to_string(),
        )
        .at_path("notes.md");
        let draft = draft_from_finding(
            OutwardKind::Issue,
            1,
            &finding,
            "owner/repo",
            None,
            &policy(),
        )
        .unwrap();
        assert!(!draft
            .body
            .contains("ghp_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789"));
        assert!(draft.body.contains(localpilot_config::redact::REDACTED));
    }

    #[test]
    fn dry_run_emit_publishes_nothing_and_mints_no_token() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let draft = draft_from_finding(
            OutwardKind::Issue,
            1,
            &todo_finding(),
            "owner/repo",
            None,
            &policy(),
        )
        .unwrap();
        draft.persist(root).unwrap();
        // Write a config enabling the feature so the emit-time allowlist re-check
        // passes; the dry run still publishes nothing.
        std::fs::write(
            root.join(".localpilot.toml"),
            "[self_improvement]\nenabled = true\noutward_targets = [\"owner/repo\"]\n",
        )
        .unwrap();

        let mut out = Vec::new();
        run_emit_draft(root, &draft.id, "david", false, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("DRY RUN"), "{text}");
        assert!(text.contains("gh issue create"), "{text}");
        // No published event was recorded (nothing ran).
        let events = root.join(format!(".localpilot/outward/{}.events.jsonl", draft.id));
        let log = std::fs::read_to_string(events).unwrap_or_default();
        assert!(
            !log.contains("published"),
            "dry run must not publish: {log}"
        );
    }

    #[test]
    fn emit_refuses_when_the_target_is_no_longer_allowlisted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let draft = draft_from_finding(
            OutwardKind::Issue,
            1,
            &todo_finding(),
            "owner/repo",
            None,
            &policy(),
        )
        .unwrap();
        draft.persist(root).unwrap();
        // No .localpilot.toml ⇒ feature disabled ⇒ emit refuses even with --approve.
        let mut out = Vec::new();
        let err = run_emit_draft(root, &draft.id, "david", true, &mut out).unwrap_err();
        assert!(err.to_string().contains("not enabled/allowlisted"), "{err}");
    }

    #[test]
    fn render_argv_quotes_multiline_body() {
        let argv = vec![
            "issue".to_string(),
            "create".to_string(),
            "--body".to_string(),
            "line one\nline two".to_string(),
        ];
        let rendered = render_argv(&argv);
        assert!(rendered.contains("issue create --body"));
        assert!(rendered.contains("\\n") || rendered.contains("line one"));
    }
}
