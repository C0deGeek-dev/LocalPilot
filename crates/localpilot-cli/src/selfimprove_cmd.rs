//! `localpilot selfimprove` — drive the human-gated self-improvement loop one
//! step at a time.
//!
//! `status` reads the current loop state from the orchestrator; `next` advances it
//! by exactly one stage: review → propose → **[human approval]** → build → reload.
//! Past the gate, `next` refuses to proceed without an explicit `--approve
//! --reviewer`, which is the deliberate human act that mints the approval token —
//! the loop never mints it itself, and there is no autonomous advance (ADR-0128).

use std::io::Write;
use std::path::Path;

use anyhow::{anyhow, Context};
use localpilot_config::{CliOverrides, ConfigPaths};
use localpilot_llm::ProviderRegistry;
use localpilot_selfimprove::{ApprovalToken, Orchestrator, SelfDevRunner, Stage};
use localpilot_selfreview::ReviewOptions;

use crate::propose_patch::{generate_proposal, proposal_branch};

/// Build the orchestrator for the repository at `repo_root`, wired to the real
/// self-dev build/reload stage rooted at the per-user data directory.
fn open_loop(repo_root: &Path) -> anyhow::Result<Orchestrator<SelfDevRunner>> {
    let selfdev_root = localpilot_selfdev::default_root()
        .ok_or_else(|| anyhow!("this platform reports no per-user data directory"))?;
    Ok(Orchestrator::open(
        repo_root,
        &selfdev_root,
        SelfDevRunner::new(&selfdev_root),
    ))
}

/// `selfimprove status`: read and print the current loop state — which stage, what
/// is pending, and whether the gate awaits a human token — via the orchestrator.
pub fn run_status(repo_root: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let loop_ = open_loop(repo_root)?;
    match loop_.state()? {
        None => {
            let report = loop_.review(&ReviewOptions::default());
            if report.findings.is_empty() {
                writeln!(out, "self-improvement loop: idle — no findings to act on.")?;
            } else {
                writeln!(
                    out,
                    "self-improvement loop: idle — {} finding(s) found. Run `localpilot \
                     selfimprove next` to propose the top finding.",
                    report.findings.len()
                )?;
            }
        }
        Some(state) => match state.stage {
            Stage::Found => {
                writeln!(
                    out,
                    "self-improvement loop: found a finding — run `next` to propose it."
                )?;
            }
            Stage::Proposed => {
                let id = state.proposal_id.as_deref().unwrap_or("(unknown)");
                writeln!(
                    out,
                    "self-improvement loop: PROPOSED `{id}` — awaiting human approval (the gate).",
                )?;
                writeln!(
                    out,
                    "  Review the diff, then cross the gate:\n    localpilot selfimprove next \
                     --approve --reviewer <you>",
                )?;
            }
            Stage::Approved => {
                writeln!(
                    out,
                    "self-improvement loop: APPROVED — run `localpilot selfimprove next` to build \
                     the approved tree.",
                )?;
            }
            Stage::Built => {
                let label = state.built_label.as_deref().unwrap_or("(unknown)");
                writeln!(
                    out,
                    "self-improvement loop: BUILT `{label}` — run `localpilot selfimprove next` to \
                     reload onto it.",
                )?;
            }
            Stage::Reloaded => {
                let label = state.built_label.as_deref().unwrap_or("(unknown)");
                writeln!(
                    out,
                    "self-improvement loop: RELOADED `{label}` — loop complete. Run `localpilot \
                     selfimprove next` to start a fresh pass.",
                )?;
            }
        },
    }
    Ok(())
}

/// Arguments for `selfimprove next` — the per-stage inputs a single advance may need.
pub struct NextArgs<'a> {
    /// The 1-based finding rank to propose (Found → Proposed).
    pub finding: usize,
    /// The model that authors the proposed edit (Found → Proposed only).
    pub model: Option<&'a str>,
    /// Provider id; the default provider is used when omitted.
    pub provider: Option<&'a str>,
    /// Cross the human gate: promote the proposed patch (the deliberate human act).
    pub approve: bool,
    /// The human reviewer recorded on the approval (required with `--approve`).
    pub reviewer: Option<&'a str>,
}

/// `selfimprove next`: advance the loop one stage via the orchestrator. Past the
/// gate it refuses to promote without an explicit `--approve --reviewer`.
pub async fn run_next(
    repo_root: &Path,
    args: &NextArgs<'_>,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let stage = open_loop(repo_root)?.state()?.map(|s| s.stage);
    match stage {
        // A fresh loop, or a completed one: review and propose the chosen finding.
        None | Some(Stage::Reloaded) => propose_step(repo_root, args, out).await,
        Some(Stage::Proposed) => gate_step(repo_root, args, out),
        Some(Stage::Approved) => build_step(repo_root, out),
        Some(Stage::Built) => reload_step(repo_root, out),
        Some(Stage::Found) => {
            // `Found` is a transient stage the loop never persists; treat it as a
            // fresh pass so `next` is always actionable.
            propose_step(repo_root, args, out).await
        }
    }
}

/// Found → Proposed: review, ask the model to author a scope-confined edit for the
/// chosen finding, and package it in an isolated worktree behind the gate.
async fn propose_step(
    repo_root: &Path,
    args: &NextArgs<'_>,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    if args.finding == 0 {
        return Err(anyhow!(
            "--finding is 1-based; use the rank shown by `localpilot self-review`"
        ));
    }
    let model = args.model.ok_or_else(|| {
        anyhow!("proposing a fix needs a model: pass `--model <name>` (and `--provider` if not the default)")
    })?;

    let config =
        localpilot_config::load(&ConfigPaths::standard(repo_root), &CliOverrides::default())?;
    let registry = ProviderRegistry::from_config(&config)?;
    let provider = match args.provider {
        Some(id) => registry
            .get(id)
            .ok_or_else(|| anyhow!("provider '{id}' is not configured"))?,
        None => registry
            .default_provider()
            .ok_or_else(|| anyhow!("no default provider is configured"))?,
    };

    let loop_ = open_loop(repo_root)?;
    let report = loop_.review(&ReviewOptions::default());
    let finding = report.findings.get(args.finding - 1).ok_or_else(|| {
        anyhow!(
            "no finding ranked {}; `localpilot self-review` lists {} finding(s)",
            args.finding,
            report.findings.len()
        )
    })?;

    let generated = generate_proposal(provider.as_ref(), model, repo_root, finding)
        .await
        .context("generating the edit")?;
    let branch = proposal_branch(args.finding);
    let proposed = loop_
        .propose(&branch, &generated.proposal, generated.provenance)
        .context("packaging the proposal in an isolated worktree")?;

    writeln!(out, "Proposed `{}` — review before promoting:", proposed.id)?;
    writeln!(out, "  files:    {}", proposed.files.join(", "))?;
    writeln!(
        out,
        "  changes:  +{} -{}",
        proposed.insertions, proposed.deletions
    )?;
    writeln!(out, "  worktree: {}", proposed.worktree.display())?;
    writeln!(out)?;
    writeln!(out, "{}", proposed.patch)?;
    writeln!(
        out,
        "Cross the human gate (approval required):\n  localpilot selfimprove next --approve \
         --reviewer <you>",
    )?;
    Ok(())
}

/// Proposed → Approved (the gate). Without `--approve` this refuses to proceed and
/// prints the required human action; with it, the reviewer's confirmation mints
/// the approval token and the orchestrator promotes.
fn gate_step(repo_root: &Path, args: &NextArgs<'_>, out: &mut dyn Write) -> anyhow::Result<()> {
    let loop_ = open_loop(repo_root)?;
    let state = loop_
        .state()?
        .ok_or_else(|| anyhow!("no active self-improvement loop"))?;
    let id = state
        .proposal_id
        .ok_or_else(|| anyhow!("the loop is at the gate but has no proposal id"))?;

    if !args.approve {
        // The structural gate: no promotion without an explicit human act. Print the
        // required action and stop, without promoting.
        writeln!(
            out,
            "Awaiting human approval — the proposed patch `{id}` will not be promoted \
             automatically.",
        )?;
        writeln!(
            out,
            "Review the diff, then cross the gate:\n  localpilot selfimprove next --approve \
             --reviewer <you>",
        )?;
        return Ok(());
    }

    let reviewer = args
        .reviewer
        .filter(|r| !r.trim().is_empty())
        .ok_or_else(|| anyhow!("--reviewer must name the human approving this patch"))?;
    // The deliberate human act mints the token; the orchestrator never mints one.
    let token = ApprovalToken::approve(&id, reviewer);
    let outcome = loop_
        .approve(&token)
        .context("promoting the approved patch onto the main branch")?;
    writeln!(
        out,
        "Promoted `{id}` onto `{}` (reviewer: {}). Run `localpilot selfimprove next` to build the \
         approved tree.",
        outcome.branch, outcome.reviewer
    )?;
    Ok(())
}

/// Approved → Built: build, vet, and promote the approved tree via the self-dev stage.
fn build_step(repo_root: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let mut loop_ = open_loop(repo_root)?;
    let record = loop_.build(out).context("building the approved tree")?;
    writeln!(
        out,
        "Built and vetted `{}`. Run `localpilot selfimprove next` to reload onto it.",
        record.label
    )?;
    Ok(())
}

/// Built → Reloaded: swap the running process onto the built binary. On success the
/// process is replaced, so this may not return.
fn reload_step(repo_root: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let mut loop_ = open_loop(repo_root)?;
    loop_
        .reload(out)
        .context("reloading onto the built binary")?;
    writeln!(out, "Reloaded. The self-improvement loop is complete.")?;
    Ok(())
}

/// `selfimprove reset`: clear the loop state so a fresh pass can start.
pub fn run_reset(repo_root: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    open_loop(repo_root)?.reset()?;
    writeln!(out, "self-improvement loop reset.")?;
    Ok(())
}
