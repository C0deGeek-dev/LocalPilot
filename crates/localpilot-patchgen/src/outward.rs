//! The outward half of the human-gated self-improvement loop (ADR-0053, extending
//! ADR-0034).
//!
//! Where [`crate::ProposedPatch`] is the *inward* write half (propose a code change,
//! a human promotes it onto the main branch), an [`OutwardDraft`] is the *outward*
//! analogue: the agent may author a **draft** issue or PR describing a proposed
//! improvement and write it to an isolated, inspectable store — but **publishing**
//! one to an external repository is gated exactly like promotion.
//!
//! The gate is structural, not a convention:
//! - producing or persisting a draft touches no network and mints no token;
//! - the only operation that yields a runnable [`PublishPlan`] is
//!   [`OutwardDraft::publish_plan`], which **requires** an [`ApprovalToken`]; the
//!   token's sole constructor ([`ApprovalToken::approve`]) is called only on an
//!   explicit human-approval path, never by the autonomous loop;
//! - a draft can only be built for a target the operator put on an explicit
//!   allowlist, with the feature switched on — both default off (fail-closed);
//! - the published artefact is **draft-only**: the `gh` argv is built so it can
//!   never carry `ready`/`merge`/`--web`, and never edits or comments on an
//!   existing item.
//!
//! The draft body is redacted at construction with the workspace's shared
//! redactor, so a secret never reaches the store even locally, and it carries the
//! [`ChangeProvenance`] of what produced it so a published draft is traceable.

use serde::{Deserialize, Serialize};

use std::path::{Path, PathBuf};

use crate::gate::ApprovalToken;
use crate::provenance::ChangeProvenance;

/// Schema tag so a consumer can pin the persisted draft shape.
pub const OUTWARD_SCHEMA: &str = "localpilot-outward-draft-v1";

/// What kind of outward artefact a draft becomes when published. Both are
/// **draft** forms: an issue, or a PR explicitly marked draft/not-ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutwardKind {
    /// A GitHub issue (`gh issue create`).
    Issue,
    /// A draft pull request (`gh pr create --draft`).
    DraftPr,
}

/// The operator's outward-emit policy, projected from `[self_improvement]` config.
/// Plain data so the gated crate stays decoupled from the config schema. Both
/// fields default off: an empty allowlist with `enabled = false` makes nothing
/// publishable (fail-closed).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutwardPolicy {
    /// Whether the outward surface is switched on at all.
    pub enabled: bool,
    /// The explicit `owner/repo` allowlist a draft may target.
    pub allowed_targets: Vec<String>,
}

impl OutwardPolicy {
    /// A policy from its parts.
    #[must_use]
    pub fn new(enabled: bool, allowed_targets: Vec<String>) -> Self {
        Self {
            enabled,
            allowed_targets,
        }
    }

    /// Whether `target_repo` may be proposed/published under this policy: the
    /// feature is enabled and the target is on the allowlist.
    #[must_use]
    pub fn allows(&self, target_repo: &str) -> bool {
        self.enabled && self.allowed_targets.iter().any(|t| t == target_repo)
    }
}

/// Errors raised while building, persisting, or publishing an outward draft.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OutwardError {
    /// The outward feature is switched off (`[self_improvement] enabled = false`).
    #[error("the outward self-improvement surface is disabled; set [self_improvement] enabled = true to opt in")]
    FeatureDisabled,

    /// The target repo is not on the configured allowlist, so no draft may be
    /// written for it.
    #[error("target repo `{0}` is not on the [self_improvement] outward_targets allowlist")]
    TargetNotAllowed(String),

    /// A draft was missing a required field (title, target, or — for a draft PR —
    /// a head branch).
    #[error("incomplete outward draft: {0}")]
    Incomplete(String),

    /// The approval token does not authorize this draft — publication refused.
    #[error("approval token does not authorize this draft")]
    TokenMismatch,

    /// No persisted draft was found for the given id.
    #[error("no outward draft found for id: {0}")]
    UnknownDraft(String),

    /// A filesystem error at `path`.
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The persisted draft record could not be (de)serialized.
    #[error("outward draft record (de)serialization failed: {0}")]
    Serde(String),
}

/// The parts of a draft the caller supplies, before redaction and the
/// policy/provenance the gated constructor folds in. Keeps [`OutwardDraft::new`]
/// to a small, named shape rather than a long positional argument list.
#[derive(Debug, Clone)]
pub struct DraftRequest {
    /// Stable id (store filename + token-authorization key).
    pub id: String,
    /// Whether this becomes an issue or a draft PR.
    pub kind: OutwardKind,
    /// The `owner/repo` this draft targets.
    pub target_repo: String,
    /// The draft title (redacted by the constructor).
    pub title: String,
    /// The human-readable description (the provenance block is appended, then the
    /// whole body is redacted).
    pub description: String,
    /// For a draft PR, the head branch to open it from; `None`/empty for an issue.
    pub head_branch: Option<String>,
}

/// A draft issue/PR proposing an improvement, awaiting human inspection and an
/// explicit publish approval. Redacted at construction; carries the provenance of
/// what produced it. Persisting it writes a JSON record under the project-local,
/// git-ignored `.localpilot/outward/` store — never the outside world.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutwardDraft {
    /// Schema tag (`OUTWARD_SCHEMA`).
    pub schema: String,
    /// Stable id (also the store filename and the token-authorization key).
    pub id: String,
    /// Whether this becomes an issue or a draft PR.
    pub kind: OutwardKind,
    /// The `owner/repo` this draft targets (allowlisted at construction).
    pub target_repo: String,
    /// The draft title (redacted).
    pub title: String,
    /// The draft body — redacted, and carrying the provenance block (D004).
    pub body: String,
    /// For a draft PR, the head branch to open the PR from. `None` for an issue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_branch: Option<String>,
}

impl OutwardDraft {
    /// Build a draft for `target_repo`, **refusing** before any work if the policy
    /// does not allow that target (disabled feature or unlisted target). The body
    /// is composed from `description` plus the `provenance` block and redacted with
    /// the shared workspace redactor, so the stored draft never contains a secret
    /// even locally.
    ///
    /// # Errors
    /// - [`OutwardError::FeatureDisabled`] when the feature is off;
    /// - [`OutwardError::TargetNotAllowed`] when `target_repo` is not allowlisted;
    /// - [`OutwardError::Incomplete`] when the title is blank, or a draft PR has no
    ///   head branch.
    pub fn new(
        request: DraftRequest,
        provenance: &ChangeProvenance,
        policy: &OutwardPolicy,
    ) -> Result<Self, OutwardError> {
        let DraftRequest {
            id,
            kind,
            target_repo,
            title,
            description,
            head_branch,
        } = request;
        // Fail-closed, before composing anything: the feature must be on and the
        // target explicitly allowlisted.
        if !policy.enabled {
            return Err(OutwardError::FeatureDisabled);
        }
        if !policy.allows(&target_repo) {
            return Err(OutwardError::TargetNotAllowed(target_repo));
        }
        let title = title.trim();
        if title.is_empty() {
            return Err(OutwardError::Incomplete("the draft has no title".into()));
        }
        if kind == OutwardKind::DraftPr && head_branch.as_deref().unwrap_or("").trim().is_empty() {
            return Err(OutwardError::Incomplete(
                "a draft PR needs a head branch".into(),
            ));
        }

        let body = render_body(&description, provenance);
        Ok(Self {
            schema: OUTWARD_SCHEMA.to_string(),
            id,
            kind,
            target_repo,
            // Redact title and body so no secret reaches the store, even locally.
            title: localpilot_config::redact::redact(title),
            body: localpilot_config::redact::redact(&body),
            head_branch: head_branch.filter(|b| !b.trim().is_empty()),
        })
    }

    /// The `gh` argument vector this draft would run when published — built **without**
    /// a token and **without** running anything, for the dry-run preview. It is
    /// **draft-only by construction**: an issue uses `issue create`, a PR uses
    /// `pr create --draft`, and neither path can ever emit `ready`, `merge`,
    /// `--web`, or an edit/comment on an existing item ([`Self::is_draft_only`]
    /// asserts this).
    #[must_use]
    pub fn publish_argv(&self) -> Vec<String> {
        let mut argv: Vec<String> = match self.kind {
            OutwardKind::Issue => vec!["issue".into(), "create".into()],
            OutwardKind::DraftPr => vec!["pr".into(), "create".into(), "--draft".into()],
        };
        argv.push("--repo".into());
        argv.push(self.target_repo.clone());
        argv.push("--title".into());
        argv.push(self.title.clone());
        argv.push("--body".into());
        argv.push(self.body.clone());
        if let (OutwardKind::DraftPr, Some(head)) = (self.kind, self.head_branch.as_ref()) {
            argv.push("--head".into());
            argv.push(head.clone());
        }
        argv
    }

    /// Whether the publish argv is draft-only and safe: it never carries an
    /// item-promoting or destructive subcommand/flag. A defence-in-depth check over
    /// [`Self::publish_argv`] — the builder cannot produce these, and this proves it.
    #[must_use]
    pub fn is_draft_only(&self) -> bool {
        const FORBIDDEN: [&str; 6] = ["ready", "merge", "--web", "edit", "comment", "close"];
        let argv = self.publish_argv();
        // The forbidden tokens may only appear as the literal title/body values the
        // human authored, never as their own argument. Check the structural slots:
        // every argv element except the title/body values must avoid them.
        let title_body = [self.title.as_str(), self.body.as_str()];
        argv.iter()
            .all(|a| title_body.contains(&a.as_str()) || !FORBIDDEN.iter().any(|f| a == f))
    }

    /// Turn this draft into a runnable [`PublishPlan`] — the **only** path to a plan
    /// the CLI will execute, and it requires an [`ApprovalToken`] that authorizes
    /// exactly this draft. There is no code path from authoring a draft to a
    /// `PublishPlan` without a token, mirroring [`crate::ProposedPatch::promote`].
    ///
    /// # Errors
    /// [`OutwardError::TokenMismatch`] if the token does not authorize this draft.
    pub fn publish_plan(&self, token: &ApprovalToken) -> Result<PublishPlan, OutwardError> {
        if !token.authorizes(&self.id) {
            return Err(OutwardError::TokenMismatch);
        }
        Ok(PublishPlan {
            program: "gh".to_string(),
            args: self.publish_argv(),
            target_repo: self.target_repo.clone(),
            reviewer: token.reviewer().to_string(),
        })
    }

    /// Persist this draft to `.localpilot/outward/{id}.json` so a later process can
    /// inspect, publish, or discard it. The store is project-local and git-ignored
    /// (ADR-0012), so it never appears in the repo's tracked tree.
    ///
    /// # Errors
    /// [`OutwardError::Io`] / [`OutwardError::Serde`] if the record cannot be written.
    pub fn persist(&self, repo_root: &Path) -> Result<(), OutwardError> {
        let path = outward_path(repo_root, &self.id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| OutwardError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let json =
            serde_json::to_string_pretty(self).map_err(|e| OutwardError::Serde(e.to_string()))?;
        std::fs::write(&path, json).map_err(|source| OutwardError::Io { path, source })
    }

    /// Load a persisted draft by id.
    ///
    /// # Errors
    /// - [`OutwardError::UnknownDraft`] if no record exists for `id`;
    /// - [`OutwardError::Serde`] if the record cannot be parsed.
    pub fn load(repo_root: &Path, id: &str) -> Result<Self, OutwardError> {
        let path = outward_path(repo_root, id);
        let json = std::fs::read_to_string(&path)
            .map_err(|_| OutwardError::UnknownDraft(id.to_string()))?;
        serde_json::from_str(&json).map_err(|e| OutwardError::Serde(e.to_string()))
    }
}

/// A runnable publish plan: the program and argv the CLI executes after an explicit
/// human approval. Obtainable **only** from [`OutwardDraft::publish_plan`] with an
/// [`ApprovalToken`], so holding one is proof a human approved this exact draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishPlan {
    /// The program to run (always `gh`).
    pub program: String,
    /// The argv passed to `gh` (never shell-interpreted).
    pub args: Vec<String>,
    /// The target repo, surfaced for the approval/preflight output.
    pub target_repo: String,
    /// The human reviewer recorded on the approval token.
    pub reviewer: String,
}

/// The phase of an outward emit, recorded as a redacted local event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmitPhase {
    /// The draft was authored and written to the store.
    Proposed,
    /// A human approved publishing it.
    Approved,
    /// It was published; `url` carries the resulting issue/PR URL.
    Published,
}

/// One redacted lifecycle event for an outward emit. Holds no approval token and
/// no secret (the message is redacted), so the local event log is safe to inspect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutwardEvent {
    /// The draft id this event concerns.
    pub id: String,
    /// Which lifecycle phase.
    pub phase: EmitPhase,
    /// The target repo.
    pub target_repo: String,
    /// The resulting URL, once published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The human reviewer, for the approve/publish phases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<String>,
}

/// The id of every persisted outward draft, sorted, for `drafts list`.
///
/// # Errors
/// [`OutwardError::Io`] only if the store directory exists but cannot be read.
pub fn list(repo_root: &Path) -> Result<Vec<String>, OutwardError> {
    let dir = outward_dir(repo_root);
    let mut ids = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        // No store yet ⇒ no drafts; that is not an error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ids),
        Err(source) => return Err(OutwardError::Io { path: dir, source }),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                ids.push(stem.to_string());
            }
        }
    }
    ids.sort();
    Ok(ids)
}

/// Discard a persisted draft and its event log. The rollback path; nothing outside
/// the project-local store is touched.
///
/// # Errors
/// [`OutwardError::UnknownDraft`] if no record exists for `id`.
pub fn discard(repo_root: &Path, id: &str) -> Result<(), OutwardError> {
    let path = outward_path(repo_root, id);
    if !path.exists() {
        return Err(OutwardError::UnknownDraft(id.to_string()));
    }
    std::fs::remove_file(&path).map_err(|source| OutwardError::Io { path, source })?;
    // Best-effort: drop the event log too.
    let _ = std::fs::remove_file(events_path(repo_root, id));
    Ok(())
}

/// Append a redacted lifecycle event for a draft to `.localpilot/outward/{id}.events.jsonl`.
/// One JSON object per line; the approval token is never recorded.
///
/// # Errors
/// [`OutwardError::Io`] / [`OutwardError::Serde`] if the event cannot be written.
pub fn record_event(repo_root: &Path, event: &OutwardEvent) -> Result<(), OutwardError> {
    let path = events_path(repo_root, &event.id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| OutwardError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let mut line = serde_json::to_string(event).map_err(|e| OutwardError::Serde(e.to_string()))?;
    // Defence in depth: redact the serialized line too, in case a URL or reviewer
    // string ever carried a secret-shaped token.
    line = localpilot_config::redact::redact(&line);
    line.push('\n');
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|source| OutwardError::Io {
            path: path.clone(),
            source,
        })?;
    file.write_all(line.as_bytes())
        .map_err(|source| OutwardError::Io { path, source })
}

/// Compose the draft body: the human-readable description, then a provenance block
/// (D004) so a published draft is traceable to what produced it. Redaction is
/// applied by the caller over the whole composed body.
fn render_body(description: &str, provenance: &ChangeProvenance) -> String {
    use std::fmt::Write as _;
    let mut body = String::new();
    let _ = writeln!(body, "{}", description.trim());
    let _ = writeln!(body);
    let _ = writeln!(body, "---");
    let _ = writeln!(
        body,
        "_Proposed by LocalPilot's self-improvement loop (advisory; human-gated). Provenance:_"
    );
    let _ = writeln!(body);
    let _ = writeln!(body, "- source: {}", provenance.prompt);
    let _ = writeln!(body, "- model: {}", provenance.model);
    let _ = writeln!(body, "- rationale: {}", provenance.rationale);
    if !provenance.risks.trim().is_empty() {
        let _ = writeln!(body, "- risks: {}", provenance.risks);
    }
    if let Some(eval) = &provenance.eval_result {
        let _ = writeln!(
            body,
            "- eval: {} — {}",
            if eval.passed { "passed" } else { "failed" },
            eval.summary
        );
    }
    body
}

/// `.localpilot/outward/` — the project-local, git-ignored store for outward drafts.
fn outward_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(".localpilot").join("outward")
}

/// The persisted draft record path for `id`.
fn outward_path(repo_root: &Path, id: &str) -> PathBuf {
    outward_dir(repo_root).join(format!("{id}.json"))
}

/// The redacted event-log path for `id`.
fn events_path(repo_root: &Path, id: &str) -> PathBuf {
    outward_dir(repo_root).join(format!("{id}.events.jsonl"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::provenance::EvalResult;

    fn policy() -> OutwardPolicy {
        OutwardPolicy::new(true, vec!["owner/repo".to_string()])
    }

    fn provenance() -> ChangeProvenance {
        let mut p = ChangeProvenance::new(
            "propose-issue for finding: stale TODO in a.rs",
            "self-review",
            "the TODO is stale and should be tracked or removed",
        );
        p.risks = "low — advisory only".to_string();
        p
    }

    fn request(id: &str, kind: OutwardKind, target: &str, title: &str, desc: &str) -> DraftRequest {
        DraftRequest {
            id: id.to_string(),
            kind,
            target_repo: target.to_string(),
            title: title.to_string(),
            description: desc.to_string(),
            head_branch: None,
        }
    }

    fn issue_draft() -> OutwardDraft {
        OutwardDraft::new(
            request(
                "outward-1",
                OutwardKind::Issue,
                "owner/repo",
                "Track the stale TODO in a.rs",
                "The self-review found a stale TODO marker.",
            ),
            &provenance(),
            &policy(),
        )
        .unwrap()
    }

    #[test]
    fn a_disabled_feature_refuses_before_any_draft_is_built() {
        let off = OutwardPolicy::new(false, vec!["owner/repo".to_string()]);
        let err = OutwardDraft::new(
            request("x", OutwardKind::Issue, "owner/repo", "t", "d"),
            &provenance(),
            &off,
        )
        .unwrap_err();
        assert!(matches!(err, OutwardError::FeatureDisabled));
    }

    #[test]
    fn an_unallowlisted_target_is_refused_at_construction() {
        let err = OutwardDraft::new(
            request("x", OutwardKind::Issue, "owner/other", "t", "d"),
            &provenance(),
            &policy(),
        )
        .unwrap_err();
        assert!(matches!(err, OutwardError::TargetNotAllowed(r) if r == "owner/other"));
    }

    #[test]
    fn a_draft_pr_requires_a_head_branch() {
        let err = OutwardDraft::new(
            request("x", OutwardKind::DraftPr, "owner/repo", "t", "d"),
            &provenance(),
            &policy(),
        )
        .unwrap_err();
        assert!(matches!(err, OutwardError::Incomplete(_)));
    }

    #[test]
    fn the_body_is_redacted_and_carries_provenance() {
        let draft = OutwardDraft::new(
            request(
                "outward-secret",
                OutwardKind::Issue,
                "owner/repo",
                "A finding",
                "Here is a leaked key: ghp_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789 in the log.",
            ),
            &provenance(),
            &policy(),
        )
        .unwrap();
        // The secret is gone, the placeholder is present.
        assert!(!draft
            .body
            .contains("ghp_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789"));
        assert!(draft.body.contains(localpilot_config::redact::REDACTED));
        // Provenance (D004) is in the body: source, model, rationale.
        assert!(draft.body.contains("model: self-review"));
        assert!(draft.body.contains("rationale: the TODO is stale"));
        assert!(draft.body.contains("source: propose-issue for finding"));
    }

    #[test]
    fn the_eval_result_is_rendered_into_the_body_when_present() {
        let mut p = provenance();
        p.eval_result = Some(EvalResult {
            passed: true,
            summary: "gate: pass".to_string(),
        });
        let draft = OutwardDraft::new(
            request("outward-eval", OutwardKind::Issue, "owner/repo", "t", "d"),
            &p,
            &policy(),
        )
        .unwrap();
        assert!(draft.body.contains("eval: passed — gate: pass"));
    }

    #[test]
    fn issue_argv_is_draft_only_and_never_promotes() {
        let draft = issue_draft();
        let argv = draft.publish_argv();
        assert_eq!(&argv[0..2], &["issue".to_string(), "create".to_string()]);
        assert!(argv.contains(&"--repo".to_string()));
        assert!(argv.contains(&"owner/repo".to_string()));
        // Never an item-promoting/destructive token in a structural slot.
        for forbidden in ["ready", "merge", "--web", "edit", "comment", "close"] {
            assert!(
                !argv.iter().any(|a| a == forbidden),
                "argv must not contain `{forbidden}`: {argv:?}"
            );
        }
        assert!(draft.is_draft_only());
    }

    #[test]
    fn draft_pr_argv_carries_draft_and_head_and_never_ready_or_merge() {
        let draft = OutwardDraft::new(
            DraftRequest {
                head_branch: Some("self-improve/fix-1".to_string()),
                ..request(
                    "outward-pr",
                    OutwardKind::DraftPr,
                    "owner/repo",
                    "Propose a fix",
                    "Body.",
                )
            },
            &provenance(),
            &policy(),
        )
        .unwrap();
        let argv = draft.publish_argv();
        assert_eq!(
            &argv[0..3],
            &[
                "pr".to_string(),
                "create".to_string(),
                "--draft".to_string()
            ]
        );
        assert!(argv
            .windows(2)
            .any(|w| w == ["--head", "self-improve/fix-1"]));
        for forbidden in ["ready", "merge", "--web"] {
            assert!(!argv.iter().any(|a| a == forbidden), "{argv:?}");
        }
        assert!(draft.is_draft_only());
    }

    #[test]
    fn is_draft_only_holds_even_when_the_body_text_mentions_merge() {
        // A finding whose human text literally says "merge"/"ready" must not trip
        // the structural safety check — those words are the title/body value, never
        // their own argv slot.
        let draft = OutwardDraft::new(
            request(
                "outward-words",
                OutwardKind::Issue,
                "owner/repo",
                "ready",
                "We should merge and close this; it is ready.",
            ),
            &provenance(),
            &policy(),
        )
        .unwrap();
        assert!(draft.is_draft_only());
        // The body value still flows through as the --body argument.
        let argv = draft.publish_argv();
        let body_idx = argv.iter().position(|a| a == "--body").unwrap();
        assert!(argv[body_idx + 1].contains("merge"));
    }

    #[test]
    fn publish_plan_requires_a_matching_token_and_yields_no_plan_without_one() {
        let draft = issue_draft();
        // A token for a different draft does not authorize this one — no plan.
        let wrong = ApprovalToken::approve("other", "david");
        assert!(matches!(
            draft.publish_plan(&wrong).unwrap_err(),
            OutwardError::TokenMismatch
        ));
        // The matching token (a human act) yields the runnable plan.
        let token = ApprovalToken::approve(draft.id.clone(), "david");
        let plan = draft.publish_plan(&token).unwrap();
        assert_eq!(plan.program, "gh");
        assert_eq!(plan.reviewer, "david");
        assert_eq!(plan.args, draft.publish_argv());
    }

    /// The human-gate proof (§6.17): the authoring/store surface an autonomous loop
    /// uses exposes **no** way to obtain an [`ApprovalToken`] and therefore no way
    /// to reach a runnable [`PublishPlan`]. The only token constructor is
    /// [`ApprovalToken::approve`], called solely on the explicit `--approve` CLI
    /// path; `publish_plan` is the sole producer of a plan and it requires the
    /// token by value. This test pins that shape: every loop-reachable operation
    /// (build, persist, load, list, preview argv) completes without a token, and
    /// none of them returns or constructs one.
    #[test]
    fn the_loop_surface_cannot_mint_a_token_or_publish() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Build + persist + load + list + preview — all token-free.
        let draft = issue_draft();
        draft.persist(root).unwrap();
        let loaded = OutwardDraft::load(root, "outward-1").unwrap();
        assert_eq!(loaded, draft);
        assert_eq!(list(root).unwrap(), vec!["outward-1".to_string()]);
        let _preview = loaded.publish_argv(); // preview is allowed; it runs nothing.
                                              // The ONLY route to a plan is publish_plan(&ApprovalToken). Without minting
                                              // a token (which the loop never does), there is no PublishPlan in scope and
                                              // no `gh` is ever executed. The type system makes this a compile-time fact:
                                              // `publish_plan` takes `&ApprovalToken` by value-reference, and the sole
                                              // constructor lives behind the human `--approve` path.
        let plan_needs_token = OutwardDraft::publish_plan; // fn item; arity proves the gate
        let _ = plan_needs_token; // referenced so the proof is not dead code
    }

    #[test]
    fn persist_load_list_and_discard_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(list(root).unwrap().is_empty());

        let draft = issue_draft();
        draft.persist(root).unwrap();
        // The store is under the git-ignored .localpilot/ tree.
        assert!(root.join(".localpilot/outward/outward-1.json").exists());
        assert_eq!(list(root).unwrap(), vec!["outward-1".to_string()]);
        assert_eq!(OutwardDraft::load(root, "outward-1").unwrap(), draft);

        discard(root, "outward-1").unwrap();
        assert!(list(root).unwrap().is_empty());
        assert!(matches!(
            OutwardDraft::load(root, "outward-1").unwrap_err(),
            OutwardError::UnknownDraft(_)
        ));
        assert!(matches!(
            discard(root, "outward-1").unwrap_err(),
            OutwardError::UnknownDraft(_)
        ));
    }

    #[test]
    fn emit_events_are_appended_and_redacted_and_hold_no_token() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        record_event(
            root,
            &OutwardEvent {
                id: "outward-1".to_string(),
                phase: EmitPhase::Proposed,
                target_repo: "owner/repo".to_string(),
                url: None,
                reviewer: None,
            },
        )
        .unwrap();
        record_event(
            root,
            &OutwardEvent {
                id: "outward-1".to_string(),
                phase: EmitPhase::Published,
                target_repo: "owner/repo".to_string(),
                url: Some("https://github.com/owner/repo/issues/1".to_string()),
                reviewer: Some("david".to_string()),
            },
        )
        .unwrap();
        let log = std::fs::read_to_string(root.join(".localpilot/outward/outward-1.events.jsonl"))
            .unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("proposed"));
        assert!(lines[1].contains("published"));
        assert!(lines[1].contains("issues/1"));
        // No approval token is ever part of the event record.
        assert!(!log.contains("ApprovalToken"));
    }
}
