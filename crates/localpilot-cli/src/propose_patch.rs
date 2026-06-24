//! `localpilot self-review propose-patch` — the write half of the self-improvement
//! loop (ADR-0034).
//!
//! Turn one approved self-review finding into a model-authored, scope-confined patch
//! proposal in an isolated git worktree (`propose-patch`), then let a human review the
//! diff and promote it onto the main branch only via an explicit approval
//! (`promote`) — or drop it (`discard`). The agent never mints the approval token,
//! never merges, and never pushes; the gate is structural in `localpilot-patchgen`.

use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context};
use clap::Subcommand;
use futures::StreamExt;
use localpilot_config::{CliOverrides, ConfigPaths};
use localpilot_core::{Message, Role};
use localpilot_llm::{ModelEvent, ModelProvider, ModelRequest, ProviderRegistry};
use localpilot_patchgen::{
    propose, ApprovalToken, ChangeProvenance, PatchProposal, ProposedEdit, ProposedPatch,
};
use localpilot_selfreview::{review, Finding, ReviewOptions};
use serde::Deserialize;

use crate::outward_cmd;

/// The `self-review` write-half subcommands.
#[derive(Debug, Subcommand)]
pub enum ProposePatchCommand {
    /// Propose a patch for one ranked self-review finding: a model authors the edit,
    /// it is written to an isolated worktree, and the command stops for human review.
    ProposePatch {
        /// The 1-based rank of the finding to patch (see `localpilot self-review`).
        #[arg(long)]
        finding: usize,
        /// The model that authors the edit.
        #[arg(long)]
        model: String,
        /// The provider id; the default provider is used when omitted.
        #[arg(long)]
        provider: Option<String>,
    },
    /// Promote a previously proposed patch onto the main branch. Requires explicit
    /// human approval — the autonomous agent loop never passes `--approve`.
    Promote {
        /// The proposal id (printed by `propose-patch`).
        #[arg(long)]
        id: String,
        /// The human reviewer recorded on the approval.
        #[arg(long)]
        reviewer: String,
        /// Explicit confirmation that a human approves promoting this patch.
        #[arg(long)]
        approve: bool,
    },
    /// Discard a previously proposed patch (remove its worktree and branch).
    Discard {
        /// The proposal id (printed by `propose-patch`).
        #[arg(long)]
        id: String,
    },
    /// Author a **draft issue** for one ranked self-review finding and write it to
    /// the local outward store for human inspection. Writes nothing to the network.
    ProposeIssue {
        /// The 1-based rank of the finding to describe (see `localpilot self-review`).
        #[arg(long)]
        finding: usize,
        /// The `owner/repo` target (must be on the `[self_improvement] outward_targets` allowlist).
        #[arg(long)]
        target: String,
    },
    /// Author a **draft PR** for one ranked self-review finding and write it to the
    /// local outward store for human inspection. Writes nothing to the network.
    ProposePr {
        /// The 1-based rank of the finding to describe (see `localpilot self-review`).
        #[arg(long)]
        finding: usize,
        /// The `owner/repo` target (must be on the `[self_improvement] outward_targets` allowlist).
        #[arg(long)]
        target: String,
        /// The head branch the draft PR would be opened from.
        #[arg(long)]
        head: String,
    },
    /// Inspect, show, or discard locally proposed outward drafts. No publish here.
    Drafts {
        #[command(subcommand)]
        command: outward_cmd::OutwardDraftsCommand,
    },
    /// Publish an inspected outward draft as a **draft** issue/PR via `gh` — only
    /// with explicit human approval. Dry-run by default (prints the plan, publishes
    /// nothing); `--approve` is the deliberate human act the autonomous loop never
    /// passes.
    EmitDraft {
        /// The draft id (printed by `propose-issue`/`propose-pr`).
        #[arg(long)]
        id: String,
        /// The human reviewer recorded on the approval.
        #[arg(long)]
        reviewer: String,
        /// Explicit confirmation that a human approves publishing this draft.
        #[arg(long)]
        approve: bool,
    },
}

/// The model's structured reply: the full new content of the file plus a one-line
/// rationale recorded in the change provenance.
#[derive(Debug, Deserialize)]
struct EditReply {
    new_content: String,
    #[serde(default)]
    rationale: String,
}

/// A generated proposal plus the provenance describing how the model produced it.
#[derive(Debug)]
pub struct GeneratedProposal {
    /// The scope-confined edits addressing the finding.
    pub proposal: PatchProposal,
    /// How and why the change was produced.
    pub provenance: ChangeProvenance,
}

/// Why generating a proposal from a finding failed.
#[derive(Debug, thiserror::Error)]
pub enum GenerateError {
    /// The finding names no file, so there is nothing to rewrite.
    #[error("this finding names no file, so it cannot be auto-patched")]
    Unpatchable,
    /// The finding's file could not be read for context.
    #[error("could not read `{path}`: {source}")]
    ReadFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// The model request failed.
    #[error("the model request failed")]
    Provider,
    /// The model reply was not the expected JSON object.
    #[error("the model reply was not the expected JSON object: {0}")]
    Malformed(String),
    /// The model proposed no change at all.
    #[error("the model proposed no change to `{0}`")]
    NoChange(String),
}

const SYSTEM_PROMPT: &str = "You fix exactly one repository-health finding by rewriting \
the single file it names. Change only what the finding requires; keep everything else \
byte-for-byte identical, and never touch any other file. Reply with ONLY a JSON object \
of shape {\"new_content\": \"<the complete new content of the file>\", \"rationale\": \
\"<one sentence on why this fixes the finding>\"} and nothing else.";

/// Generate a scope-confined patch proposal for `finding` by asking `provider` to
/// rewrite the single file the finding names. The proposal touches only that file;
/// `localpilot-patchgen` re-checks scope when it packages the patch.
pub async fn generate_proposal(
    provider: &dyn ModelProvider,
    model: &str,
    repo_root: &Path,
    finding: &Finding,
) -> Result<GeneratedProposal, GenerateError> {
    let path = finding.path.clone().ok_or(GenerateError::Unpatchable)?;
    let current = std::fs::read_to_string(repo_root.join(&path)).map_err(|source| {
        GenerateError::ReadFile {
            path: path.clone(),
            source,
        }
    })?;

    let user = format!(
        "Finding: {evidence}\n\nFile (project-relative): {path}\n\nCurrent content of \
         {path}:\n```\n{current}\n```\n\nRewrite {path} to address the finding.",
        evidence = finding.evidence,
    );
    let request = ModelRequest::new(
        model,
        vec![
            Message::text(Role::System, SYSTEM_PROMPT),
            Message::text(Role::User, user),
        ],
    );

    let reply = collect_text(provider, request).await?;
    let parsed = parse_reply(&reply)?;
    if parsed.new_content == current {
        return Err(GenerateError::NoChange(path));
    }

    let rationale = if parsed.rationale.trim().is_empty() {
        format!("Address the self-review finding: {}", finding.evidence)
    } else {
        parsed.rationale
    };
    let provenance = ChangeProvenance::new(
        format!("propose-patch for finding: {}", finding.evidence),
        model,
        rationale,
    );
    let proposal = PatchProposal::new(
        finding.evidence.clone(),
        vec![path.clone()],
        vec![ProposedEdit::new(path, parsed.new_content)],
    );
    Ok(GeneratedProposal {
        proposal,
        provenance,
    })
}

async fn collect_text(
    provider: &dyn ModelProvider,
    request: ModelRequest,
) -> Result<String, GenerateError> {
    let mut stream = provider
        .stream(request)
        .await
        .map_err(|_| GenerateError::Provider)?;
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        match event {
            Ok(ModelEvent::TextDelta(delta)) => text.push_str(&delta),
            Ok(ModelEvent::Done) => break,
            Ok(_) => {}
            Err(_) => return Err(GenerateError::Provider),
        }
    }
    Ok(text)
}

/// Extract the first complete JSON object from a possibly fenced/prose-wrapped reply.
fn parse_reply(reply: &str) -> Result<EditReply, GenerateError> {
    let start = reply
        .find('{')
        .ok_or_else(|| GenerateError::Malformed(truncate(reply)))?;
    let end = reply
        .rfind('}')
        .ok_or_else(|| GenerateError::Malformed(truncate(reply)))?;
    if end < start {
        return Err(GenerateError::Malformed(truncate(reply)));
    }
    serde_json::from_str(&reply[start..=end]).map_err(|e| GenerateError::Malformed(e.to_string()))
}

fn truncate(s: &str) -> String {
    s.trim().chars().take(120).collect()
}

/// A safe, collision-resistant branch/worktree id for a proposal.
fn proposal_branch(finding_rank: usize) -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("self-review-{finding_rank}-{secs}")
}

/// Dispatch a `self-review` write-half subcommand.
pub async fn dispatch(cmd: ProposePatchCommand, out: &mut dyn Write) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    match cmd {
        ProposePatchCommand::ProposePatch {
            finding,
            model,
            provider,
        } => run_propose(&cwd, finding, &model, provider.as_deref(), out).await,
        ProposePatchCommand::Promote {
            id,
            reviewer,
            approve,
        } => run_promote(&cwd, &id, &reviewer, approve, out),
        ProposePatchCommand::Discard { id } => run_discard(&cwd, &id, out),
        ProposePatchCommand::ProposeIssue { finding, target } => {
            outward_cmd::run_propose_issue(&cwd, finding, &target, out)
        }
        ProposePatchCommand::ProposePr {
            finding,
            target,
            head,
        } => outward_cmd::run_propose_pr(&cwd, finding, &target, &head, out),
        ProposePatchCommand::Drafts { command } => outward_cmd::run_drafts(&cwd, command, out),
        ProposePatchCommand::EmitDraft {
            id,
            reviewer,
            approve,
        } => outward_cmd::run_emit_draft(&cwd, &id, &reviewer, approve, out),
    }
}

async fn run_propose(
    repo_root: &Path,
    finding_rank: usize,
    model: &str,
    provider_id: Option<&str>,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    if finding_rank == 0 {
        return Err(anyhow!(
            "--finding is 1-based; use the rank shown by `localpilot self-review`"
        ));
    }
    let config =
        localpilot_config::load(&ConfigPaths::standard(repo_root), &CliOverrides::default())?;
    let registry = ProviderRegistry::from_config(&config)?;
    let provider = match provider_id {
        Some(id) => registry
            .get(id)
            .ok_or_else(|| anyhow!("provider '{id}' is not configured"))?,
        None => registry
            .default_provider()
            .ok_or_else(|| anyhow!("no default provider is configured"))?,
    };

    let report = review(
        repo_root,
        &ReviewOptions {
            prior_lessons: Vec::new(),
            friction_block: None,
            process: None,
            include_missing_tests: false,
            include_cleanup: false,
        },
    );
    let finding = report.findings.get(finding_rank - 1).ok_or_else(|| {
        anyhow!(
            "no finding ranked {finding_rank}; `localpilot self-review` lists {} finding(s)",
            report.findings.len()
        )
    })?;

    let generated = generate_proposal(provider.as_ref(), model, repo_root, finding)
        .await
        .context("generating the edit")?;
    let branch = proposal_branch(finding_rank);
    let patch = propose(
        repo_root,
        &branch,
        &generated.proposal,
        generated.provenance,
    )
    .context("packaging the proposal in an isolated worktree")?;

    let summary = patch.diff_summary();
    writeln!(
        out,
        "Proposed patch `{}` — review before promoting:",
        patch.id()
    )?;
    writeln!(out, "  files:    {}", summary.files.join(", "))?;
    writeln!(
        out,
        "  changes:  +{} -{}",
        summary.insertions, summary.deletions
    )?;
    writeln!(out, "  worktree: {}", patch.worktree_path().display())?;
    writeln!(out)?;
    writeln!(out, "{}", summary.patch)?;
    writeln!(
        out,
        "Promote (human approval required):\n  localpilot self-review promote --id {} --reviewer <you> --approve",
        patch.id()
    )?;
    writeln!(
        out,
        "Discard:\n  localpilot self-review discard --id {}",
        patch.id()
    )?;

    // Leave the proposal on disk so the human can review it and promote/discard later.
    patch
        .persist()
        .context("persisting the proposal for later review")?;
    Ok(())
}

fn run_promote(
    repo_root: &Path,
    id: &str,
    reviewer: &str,
    approve: bool,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    // The structural gate: no promotion without an explicit human act. `--approve`
    // is the deliberate confirmation; the autonomous loop never sets it.
    if !approve {
        return Err(anyhow!(
            "refusing to promote without explicit human approval; re-run with --approve --reviewer <you>"
        ));
    }
    if reviewer.trim().is_empty() {
        return Err(anyhow!(
            "--reviewer must name the human approving this patch"
        ));
    }
    let patch = ProposedPatch::reopen(repo_root, id).context("reopening the proposed patch")?;
    let token = ApprovalToken::approve(patch.id(), reviewer);
    let outcome = patch
        .promote(&token)
        .context("promoting the patch onto the main branch")?;
    writeln!(
        out,
        "Promoted `{id}` onto `{}` (reviewer: {}).",
        outcome.branch, outcome.reviewer
    )?;
    Ok(())
}

fn run_discard(repo_root: &Path, id: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    let patch = ProposedPatch::reopen(repo_root, id).context("reopening the proposed patch")?;
    patch.discard().context("discarding the proposed patch")?;
    writeln!(out, "Discarded `{id}` (worktree and branch removed).")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_llm::FakeProvider;
    use localpilot_selfreview::{FindingKind, Severity};

    fn finding_at(path: &str) -> Finding {
        Finding::new(
            FindingKind::Todo,
            Severity::Low,
            0.9,
            "stale TODO marker".to_string(),
        )
        .at_path(path)
    }

    #[tokio::test]
    async fn generates_a_scope_confined_proposal_from_a_finding() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn f() {} // TODO\n").unwrap();
        let provider = FakeProvider::new()
            .text("{\"new_content\": \"fn f() {}\\n\", \"rationale\": \"drop the stale TODO\"}");
        let finding = finding_at("a.rs");

        let generated = generate_proposal(&provider, "fake", dir.path(), &finding)
            .await
            .unwrap();
        assert_eq!(generated.proposal.allowed_paths, vec!["a.rs".to_string()]);
        assert_eq!(generated.proposal.edits.len(), 1);
        assert_eq!(generated.proposal.edits[0].path, "a.rs");
        assert!(!generated.proposal.edits[0].new_content.contains("TODO"));
        assert!(generated.provenance.is_complete());
        // Scope is structural: only the finding's file is ever in the proposal.
        generated.proposal.validate_scope().unwrap();
    }

    #[tokio::test]
    async fn a_finding_without_a_path_is_unpatchable() {
        let dir = tempfile::tempdir().unwrap();
        let provider = FakeProvider::new().text("{}");
        let finding = Finding::new(FindingKind::Friction, Severity::Low, 0.5, "no file".into());
        let err = generate_proposal(&provider, "fake", dir.path(), &finding)
            .await
            .unwrap_err();
        assert!(matches!(err, GenerateError::Unpatchable));
    }

    #[tokio::test]
    async fn a_malformed_model_reply_is_a_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "x\n").unwrap();
        let provider = FakeProvider::new().text("not json at all");
        let err = generate_proposal(&provider, "fake", dir.path(), &finding_at("a.rs"))
            .await
            .unwrap_err();
        assert!(matches!(err, GenerateError::Malformed(_)));
    }

    #[tokio::test]
    async fn a_no_op_rewrite_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "same\n").unwrap();
        let provider = FakeProvider::new().text("{\"new_content\": \"same\\n\"}");
        let err = generate_proposal(&provider, "fake", dir.path(), &finding_at("a.rs"))
            .await
            .unwrap_err();
        assert!(matches!(err, GenerateError::NoChange(_)));
    }

    #[test]
    fn promote_refuses_without_explicit_human_approval() {
        // The gate invariant: no token is minted and nothing is touched when the
        // human has not explicitly approved. This fails before any reopen.
        let dir = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let err = run_promote(dir.path(), "any-id", "david", false, &mut out).unwrap_err();
        assert!(err.to_string().contains("explicit human approval"));
        assert!(out.is_empty());
    }

    #[test]
    fn promote_refuses_an_empty_reviewer() {
        let dir = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let err = run_promote(dir.path(), "any-id", "  ", true, &mut out).unwrap_err();
        assert!(err.to_string().contains("--reviewer"));
    }
}
