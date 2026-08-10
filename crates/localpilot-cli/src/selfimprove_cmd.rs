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
use localpilot_patchgen::ProposedPatch;
use localpilot_selfimprove::{ApprovalToken, Orchestrator, SelfDevRunner, Stage};
use localpilot_selfreview::{Finding, ReviewOptions};
#[cfg(feature = "tui")]
use localpilot_slash::SelfImproveAction;

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

/// The kind of work an interactive self-improvement action will perform. Hosts
/// use this preflight only to choose cancellation and confirmation policy; the
/// persisted orchestrator remains the sole authority for the actual transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(feature = "tui")]
pub(crate) enum InteractiveStep {
    Read,
    Propose,
    Gate,
    Approve,
    Build,
    Reload,
    Reset,
}

/// Result of one chat action. Reload is deliberately deferred until the host has
/// restored terminal modes; every other result leaves the current chat running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(feature = "tui")]
pub(crate) enum InteractiveOutcome {
    Complete,
    DeferredReload,
}

/// Resolve the live persisted stage into host policy without advancing it.
#[cfg(feature = "tui")]
pub(crate) fn interactive_step(
    repo_root: &Path,
    action: &SelfImproveAction,
) -> anyhow::Result<InteractiveStep> {
    let stage = open_loop(repo_root)?.state()?.map(|state| state.stage);
    Ok(interactive_step_for(stage, action))
}

#[cfg(feature = "tui")]
fn interactive_step_for(stage: Option<Stage>, action: &SelfImproveAction) -> InteractiveStep {
    match action {
        SelfImproveAction::Status => InteractiveStep::Read,
        SelfImproveAction::Reset => InteractiveStep::Reset,
        SelfImproveAction::Approve { .. } => InteractiveStep::Approve,
        SelfImproveAction::Start { .. } => match stage {
            None | Some(Stage::Found | Stage::Reloaded) => InteractiveStep::Propose,
            Some(Stage::Proposed) => InteractiveStep::Gate,
            Some(Stage::Approved | Stage::Built) => InteractiveStep::Read,
        },
        SelfImproveAction::Next => match stage {
            None | Some(Stage::Found | Stage::Reloaded) => InteractiveStep::Propose,
            Some(Stage::Proposed) => InteractiveStep::Gate,
            Some(Stage::Approved) => InteractiveStep::Build,
            Some(Stage::Built) => InteractiveStep::Reload,
        },
    }
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

#[cfg(feature = "tui")]
const MAX_CHAT_FINDINGS: usize = 50;
#[cfg(feature = "tui")]
const MAX_CHAT_EVIDENCE_CHARS: usize = 256;
#[cfg(feature = "tui")]
const MAX_CHAT_BUILD_BYTES: usize = 128 * 1024;

#[cfg(feature = "tui")]
fn clipped(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let clipped: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{clipped}…")
    } else {
        clipped
    }
}

#[cfg(feature = "tui")]
fn render_finding(rank: usize, finding: &Finding, out: &mut dyn Write) -> std::io::Result<()> {
    let location = match (&finding.path, &finding.span) {
        (Some(path), Some(span)) => {
            format!(
                "{}:{}",
                clipped(path, MAX_CHAT_EVIDENCE_CHARS),
                span.start_line
            )
        }
        (Some(path), None) => clipped(path, MAX_CHAT_EVIDENCE_CHARS),
        _ => "-".to_string(),
    };
    writeln!(
        out,
        "{rank}. [{:?}/{:?} {:.2} risk:{:?}] {location}: {}",
        finding.severity,
        finding.kind,
        finding.confidence,
        finding.risk,
        clipped(&finding.evidence, MAX_CHAT_EVIDENCE_CHARS),
    )?;
    if let Some(recommendation) = &finding.recommendation {
        writeln!(
            out,
            "   recommend: {}",
            clipped(recommendation, MAX_CHAT_EVIDENCE_CHARS)
        )?;
    }
    Ok(())
}

#[cfg(feature = "tui")]
fn render_findings(
    report: &localpilot_selfreview::Report,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(
        out,
        "self-review: {} finding(s) across {} file(s)",
        report.findings.len(),
        report.scanned_files
    )?;
    for (index, finding) in report.findings.iter().take(MAX_CHAT_FINDINGS).enumerate() {
        render_finding(index + 1, finding, out)?;
    }
    if report.findings.len() > MAX_CHAT_FINDINGS {
        writeln!(
            out,
            "… {} additional finding(s) omitted; use `localpilot self-review` for the complete report.",
            report.findings.len() - MAX_CHAT_FINDINGS
        )?;
    }
    Ok(())
}

/// Render the exact persisted proposal currently behind the gate. Both chat
/// hosts call this before their explicit approval dialog, and the gate path
/// reopens the same id before minting the token.
pub(crate) fn render_pending_proposal(
    repo_root: &Path,
    out: &mut dyn Write,
) -> anyhow::Result<String> {
    let state = open_loop(repo_root)?
        .state()?
        .ok_or_else(|| anyhow!("no active self-improvement loop"))?;
    if state.stage != Stage::Proposed {
        return Err(anyhow!(
            "self-improvement approval is only valid at PROPOSED (current stage: {:?})",
            state.stage
        ));
    }
    let id = state
        .proposal_id
        .ok_or_else(|| anyhow!("the loop is at the gate but has no proposal id"))?;
    let proposal = ProposedPatch::reopen(repo_root, &id)
        .context("reopening the persisted proposal for review")?;
    let summary = proposal.diff_summary();
    writeln!(out, "Proposal `{id}` awaiting human approval:")?;
    writeln!(out, "  files:    {}", summary.files.join(", "))?;
    writeln!(
        out,
        "  changes:  +{} -{}",
        summary.insertions, summary.deletions
    )?;
    writeln!(out, "  worktree: {}", proposal.worktree_path().display())?;
    writeln!(out)?;
    writeln!(out, "{}", summary.patch)?;
    Ok(id)
}

#[cfg(feature = "tui")]
struct CappedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

#[cfg(feature = "tui")]
impl CappedOutput {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            truncated: false,
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.truncated {
            self.bytes
                .extend_from_slice(b"\n[build output truncated at 128 KiB]\n");
        }
        self.bytes
    }
}

#[cfg(feature = "tui")]
impl Write for CappedOutput {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let remaining = MAX_CHAT_BUILD_BYTES.saturating_sub(self.bytes.len());
        let admitted = remaining.min(buffer.len());
        self.bytes.extend_from_slice(&buffer[..admitted]);
        self.truncated |= admitted < buffer.len();
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Execute one interactive action through the same CLI stage functions and the
/// same persisted orchestrator. The only special result is a reload request:
/// the host must restore its terminal before calling [`reload_after_chat`].
#[cfg(feature = "tui")]
pub(crate) async fn run_interactive(
    repo_root: &Path,
    action: &SelfImproveAction,
    expected_step: InteractiveStep,
    confirmed_proposal_id: Option<&str>,
    model: &str,
    provider: &str,
    out: &mut dyn Write,
) -> anyhow::Result<InteractiveOutcome> {
    let actual_step = interactive_step(repo_root, action)?;
    if actual_step != expected_step {
        return Err(anyhow!(
            "self-improvement state changed before the action ran (expected {expected_step:?}, now {actual_step:?}); inspect `/selfimprove status` and retry"
        ));
    }
    match action {
        SelfImproveAction::Status => run_interactive_status(repo_root, out).await?,
        SelfImproveAction::Reset => run_reset(repo_root, out)?,
        SelfImproveAction::Approve { reviewer } => {
            approve_step(repo_root, reviewer, confirmed_proposal_id, false, out)?;
        }
        SelfImproveAction::Start { finding } => {
            run_interactive_start(repo_root, *finding, model, provider, out).await?;
        }
        SelfImproveAction::Next => {
            let stage = open_loop(repo_root)?.state()?.map(|state| state.stage);
            match stage {
                None | Some(Stage::Found | Stage::Reloaded) => {
                    run_interactive_start(repo_root, None, model, provider, out).await?;
                }
                Some(Stage::Proposed) => {
                    render_pending_proposal(repo_root, out)?;
                    writeln!(
                        out,
                        "Awaiting human approval — review the diff, then run `/selfimprove approve <reviewer>`."
                    )?;
                }
                Some(Stage::Approved) => {
                    let root = repo_root.to_path_buf();
                    let (result, bytes) = tokio::task::spawn_blocking(move || {
                        let mut captured = CappedOutput::new();
                        let result = build_step(&root, &mut captured);
                        (result, captured.finish())
                    })
                    .await
                    .context("joining the self-improvement build")?;
                    out.write_all(&bytes)?;
                    result?;
                }
                Some(Stage::Built) => {
                    writeln!(
                        out,
                        "Reload confirmed — restoring the terminal before replacing the running LocalPilot."
                    )?;
                    return Ok(InteractiveOutcome::DeferredReload);
                }
            }
        }
    }
    Ok(InteractiveOutcome::Complete)
}

#[cfg(feature = "tui")]
async fn run_interactive_status(repo_root: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    match open_loop(repo_root)?.state()? {
        None => {
            let root = repo_root.to_path_buf();
            let report = tokio::task::spawn_blocking(move || {
                let loop_ = open_loop(&root)?;
                Ok::<_, anyhow::Error>(loop_.review(&ReviewOptions::default()))
            })
            .await
            .context("joining self-improvement status review")??;
            if report.findings.is_empty() {
                writeln!(out, "self-improvement loop: idle — no findings to act on.")?;
            } else {
                writeln!(
                    out,
                    "self-improvement loop: idle — {} finding(s) found. Run `/selfimprove start` to review and select one.",
                    report.findings.len()
                )?;
            }
        }
        Some(state) => match state.stage {
            Stage::Found => writeln!(
                out,
                "self-improvement loop: finding ready — run `/selfimprove start` to propose it."
            )?,
            Stage::Proposed => {
                let id = state.proposal_id.as_deref().unwrap_or("(unknown)");
                writeln!(
                    out,
                    "self-improvement loop: PROPOSED `{id}` — awaiting human approval."
                )?;
                writeln!(
                    out,
                    "Run `/selfimprove next` to review the diff, then `/selfimprove approve <reviewer>` to cross the gate."
                )?;
            }
            Stage::Approved => writeln!(
                out,
                "self-improvement loop: APPROVED — run `/selfimprove next` to build the approved tree."
            )?,
            Stage::Built => {
                let label = state.built_label.as_deref().unwrap_or("(unknown)");
                writeln!(
                    out,
                    "self-improvement loop: BUILT `{label}` — run `/selfimprove next` to confirm reload."
                )?;
            }
            Stage::Reloaded => {
                let label = state.built_label.as_deref().unwrap_or("(unknown)");
                writeln!(
                    out,
                    "self-improvement loop: RELOADED `{label}` — complete. Run `/selfimprove start` for a fresh pass."
                )?;
            }
        },
    }
    Ok(())
}

#[cfg(feature = "tui")]
async fn run_interactive_start(
    repo_root: &Path,
    finding: Option<usize>,
    model: &str,
    provider: &str,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let stage = open_loop(repo_root)?.state()?.map(|state| state.stage);
    if let Some(stage @ (Stage::Proposed | Stage::Approved | Stage::Built)) = stage {
        if stage == Stage::Proposed {
            render_pending_proposal(repo_root, out)?;
            writeln!(
                out,
                "This proposal is already active; run `/selfimprove approve <reviewer>` after review."
            )?;
        } else {
            run_interactive_status(repo_root, out).await?;
        }
        return Ok(());
    }

    let root = repo_root.to_path_buf();
    let report = tokio::task::spawn_blocking(move || {
        let loop_ = open_loop(&root)?;
        Ok::<_, anyhow::Error>(loop_.review(&ReviewOptions::default()))
    })
    .await
    .context("joining the self-review scan")??;
    if report.findings.is_empty() {
        writeln!(out, "self-improvement loop: idle — no findings to act on.")?;
        return Ok(());
    }
    render_findings(&report, out)?;
    let selected = match (finding, report.findings.len()) {
        (Some(rank), _) => rank,
        (None, 1) => 1,
        (None, count) => {
            writeln!(
                out,
                "Select one finding explicitly with `/selfimprove start <rank>` (1-{count}); no proposal was created."
            )?;
            return Ok(());
        }
    };
    if selected == 0 || selected > report.findings.len() {
        return Err(anyhow!(
            "finding rank {selected} is outside the available range 1-{}",
            report.findings.len()
        ));
    }
    writeln!(
        out,
        "Selected finding {selected}; generating an isolated proposal…"
    )?;
    propose_selected(
        repo_root,
        &NextArgs {
            finding: selected,
            model: Some(model),
            provider: Some(provider),
            approve: false,
            reviewer: None,
        },
        &report.findings[selected - 1],
        out,
    )
    .await
}

/// Complete a previously confirmed chat reload after terminal restoration.
#[cfg(feature = "tui")]
pub(crate) fn reload_after_chat(repo_root: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    reload_step(repo_root, out)
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
    let loop_ = open_loop(repo_root)?;
    let report = loop_.review(&ReviewOptions::default());
    let finding = report.findings.get(args.finding - 1).ok_or_else(|| {
        anyhow!(
            "no finding ranked {}; `localpilot self-review` lists {} finding(s)",
            args.finding,
            report.findings.len()
        )
    })?;

    propose_selected(repo_root, args, finding, out).await
}

async fn propose_selected(
    repo_root: &Path,
    args: &NextArgs<'_>,
    finding: &Finding,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
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
    let generated = generate_proposal(provider.as_ref(), model, repo_root, finding)
        .await
        .context("generating the edit")?;
    let branch = proposal_branch(args.finding);
    let loop_ = open_loop(repo_root)?;
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
    approve_step(repo_root, reviewer, None, true, out)
}

/// The single CLI/chat approval seam. It reopens the persisted proposal, binds
/// the human reviewer to that exact id, mints the only approval token, and hands
/// it to the orchestrator. `show_proposal` is false only when a chat host already
/// rendered the same persisted summary before its confirmation dialog.
fn approve_step(
    repo_root: &Path,
    reviewer: &str,
    expected_id: Option<&str>,
    show_proposal: bool,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let reviewer = reviewer.trim();
    if reviewer.is_empty() {
        return Err(anyhow!("reviewer must name the human approving this patch"));
    }
    if show_proposal {
        render_pending_proposal(repo_root, out)?;
    }
    let loop_ = open_loop(repo_root)?;
    let state = loop_
        .state()?
        .ok_or_else(|| anyhow!("no active self-improvement loop"))?;
    if state.stage != Stage::Proposed {
        return Err(anyhow!(
            "self-improvement approval is only valid at PROPOSED (current stage: {:?})",
            state.stage
        ));
    }
    let id = state
        .proposal_id
        .ok_or_else(|| anyhow!("the loop is at the gate but has no proposal id"))?;
    if expected_id.is_some_and(|expected| expected != id) {
        return Err(anyhow!(
            "the proposal changed after it was displayed; inspect `/selfimprove status` and approve the current diff explicitly"
        ));
    }
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

#[cfg(all(test, feature = "tui"))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn interactive_next_never_crosses_the_human_gate() {
        assert_eq!(
            interactive_step_for(Some(Stage::Proposed), &SelfImproveAction::Next),
            InteractiveStep::Gate
        );
        assert_eq!(
            interactive_step_for(
                Some(Stage::Proposed),
                &SelfImproveAction::Approve {
                    reviewer: "a human".to_string(),
                }
            ),
            InteractiveStep::Approve
        );
        for stage in [
            None,
            Some(Stage::Found),
            Some(Stage::Approved),
            Some(Stage::Built),
            Some(Stage::Reloaded),
        ] {
            assert_ne!(
                interactive_step_for(stage, &SelfImproveAction::Next),
                InteractiveStep::Approve,
                "next minted approval authority at {stage:?}"
            );
        }
    }

    #[test]
    fn interactive_stage_policy_is_one_step_and_start_does_not_skip_a_live_loop() {
        assert_eq!(
            interactive_step_for(None, &SelfImproveAction::Start { finding: None }),
            InteractiveStep::Propose
        );
        assert_eq!(
            interactive_step_for(Some(Stage::Approved), &SelfImproveAction::Next),
            InteractiveStep::Build
        );
        assert_eq!(
            interactive_step_for(Some(Stage::Built), &SelfImproveAction::Next),
            InteractiveStep::Reload
        );
        assert_eq!(
            interactive_step_for(
                Some(Stage::Approved),
                &SelfImproveAction::Start { finding: Some(1) }
            ),
            InteractiveStep::Read
        );
        assert_eq!(
            interactive_step_for(
                Some(Stage::Built),
                &SelfImproveAction::Start { finding: Some(1) }
            ),
            InteractiveStep::Read
        );
    }

    #[test]
    fn chat_build_capture_is_bounded_and_marks_truncation() {
        let mut output = CappedOutput::new();
        let bytes = vec![b'x'; MAX_CHAT_BUILD_BYTES + 17];
        assert_eq!(output.write(&bytes).unwrap(), bytes.len());
        let captured = output.finish();
        assert!(captured.starts_with(&bytes[..MAX_CHAT_BUILD_BYTES]));
        assert!(
            String::from_utf8_lossy(&captured).ends_with("[build output truncated at 128 KiB]\n")
        );
    }

    #[test]
    fn finding_text_clipping_preserves_utf8_boundaries() {
        assert_eq!(clipped("a界b", 2), "a界…");
        assert_eq!(clipped("a界", 2), "a界");
    }
}
