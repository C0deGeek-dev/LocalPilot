//! The cross-context **handoff** artifact and its deterministic **resume check**.
//!
//! A handoff captures enough durable session state for a fresh agent to pick up
//! work: a small machine-checkable header (id, repo, branch, commit, dirty,
//! session, references, …) plus a human-readable Markdown body. It is written from
//! the session event log and the harness documents (`brief.md` / `PROGRESS.md` /
//! `DECISIONS.md`) — never the raw transcript — and **references** those documents
//! by path rather than duplicating them. The whole artifact is redacted through
//! the canonical host redactor before it touches disk.
//!
//! It is an **execution record**: stored under `.localpilot/handoffs/<id>.md`
//! (git-ignored, never committed), distinct from the harness `brief.md`/`PROGRESS.md`
//! runtime files, and **never** promoted to LocalMind accepted memory.
//!
//! The resume check is deterministic and never asks a model to judge prose: it
//! reads the header and reports whether the repo still matches (branch identity,
//! commit existence, dirty-state, referenced paths, referenced session). A
//! mismatch is a *flag for re-verification*, not a hard failure — stale facts are
//! surfaced as warnings, not dropped.

use std::path::{Path, PathBuf};
use std::process::Command;

use localpilot_config::redact::redact;
use localpilot_core::SessionId;
use localpilot_store::{SessionEventKind, Store};

use crate::brief::Brief;
use crate::decisions::today;
use crate::error::HarnessError;
use crate::progress::Progress;

/// The schema tag stamped into every handoff header, so a reader can reject a
/// shape it does not understand.
pub const HANDOFF_SCHEMA: &str = "localpilot-handoff/1";

/// The git-ignored directory (under `.localpilot/`) handoffs are written to.
const HANDOFF_DIR: &str = "handoffs";

/// The machine-checkable header of a handoff: every field the deterministic resume
/// check reads lives here (in the header, not buried in the prose body).
#[derive(Debug, Clone, PartialEq)]
pub struct HandoffHeader {
    pub schema: String,
    pub id: String,
    pub created: String,
    pub repo: String,
    pub branch: String,
    pub commit: String,
    pub dirty: bool,
    /// The session this handoff was written from, if any (for the resume check to
    /// confirm the session still exists).
    pub session: Option<String>,
    pub confidence: f32,
    pub objective: String,
    pub next_action: String,
    /// Harness documents referenced (not duplicated), by path.
    pub references: Vec<String>,
    /// Skills suggested for the next session.
    pub suggested_skills: Vec<String>,
}

/// A parsed handoff: its header plus the Markdown body after the `---` divider.
#[derive(Debug, Clone, PartialEq)]
pub struct Handoff {
    pub header: HandoffHeader,
    pub body: String,
}

impl HandoffHeader {
    /// Render the header as a deterministic `key: value` block.
    #[must_use]
    pub fn render(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(out, "schema: {}", self.schema);
        let _ = writeln!(out, "id: {}", self.id);
        let _ = writeln!(out, "created: {}", self.created);
        let _ = writeln!(out, "repo: {}", self.repo);
        let _ = writeln!(out, "branch: {}", self.branch);
        let _ = writeln!(out, "commit: {}", self.commit);
        let _ = writeln!(out, "dirty: {}", self.dirty);
        let _ = writeln!(out, "session: {}", self.session.as_deref().unwrap_or(""));
        let _ = writeln!(out, "confidence: {:.2}", self.confidence);
        let _ = writeln!(out, "objective: {}", one_line(&self.objective));
        let _ = writeln!(out, "next_action: {}", one_line(&self.next_action));
        let _ = writeln!(out, "references: {}", self.references.join(", "));
        let _ = writeln!(
            out,
            "suggested_skills: {}",
            self.suggested_skills.join(", ")
        );
        out
    }
}

impl Handoff {
    /// Render the whole artifact: the header block, a `---` divider, then the body.
    #[must_use]
    pub fn render(&self) -> String {
        format!("{}---\n\n{}", self.header.render(), self.body)
    }

    /// Parse a handoff artifact (header block + `---` + body).
    ///
    /// # Errors
    /// Returns [`HarnessError::Malformed`] if the header is missing the divider or
    /// the schema/required fields.
    pub fn parse(text: &str) -> Result<Self, HarnessError> {
        let text = text.replace("\r\n", "\n");
        let (header_block, body) =
            text.split_once("\n---")
                .ok_or_else(|| HarnessError::Malformed {
                    document: "handoff",
                    detail: "missing '---' divider between header and body".to_string(),
                })?;
        let mut fields = std::collections::BTreeMap::new();
        for line in header_block.lines() {
            if let Some((key, value)) = line.split_once(':') {
                fields.insert(key.trim().to_string(), value.trim().to_string());
            }
        }
        let get = |key: &str| fields.get(key).cloned().unwrap_or_default();
        let schema = get("schema");
        if schema != HANDOFF_SCHEMA {
            return Err(HarnessError::Malformed {
                document: "handoff",
                detail: format!("unsupported handoff schema {schema:?}"),
            });
        }
        let list = |key: &str| -> Vec<String> {
            let raw = get(key);
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        };
        let session = {
            let s = get("session");
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        };
        let header = HandoffHeader {
            schema,
            id: get("id"),
            created: get("created"),
            repo: get("repo"),
            branch: get("branch"),
            commit: get("commit"),
            dirty: get("dirty") == "true",
            session,
            confidence: get("confidence").parse().unwrap_or(0.0),
            objective: get("objective"),
            next_action: get("next_action"),
            references: list("references"),
            suggested_skills: list("suggested_skills"),
        };
        Ok(Self {
            header,
            body: body
                .trim_start_matches('\n')
                .trim_start_matches("---")
                .to_string(),
        })
    }
}

/// The outcome of writing a handoff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffSummary {
    pub id: String,
    pub path: PathBuf,
}

/// Write a handoff for `session` under `root`, gathering durable state from the
/// session event log and the harness documents, redacting the whole artifact, and
/// storing it git-ignored under `.localpilot/handoffs/<id>.md`.
///
/// `objective` overrides the derived objective when set; `suggested_skills` is the
/// host-supplied list of skills to suggest for the next session (kept out of the
/// harness so it need not depend on the skill loader).
///
/// # Errors
/// Returns [`HarnessError`] if a referenced document is malformed or the artifact
/// cannot be written.
pub fn write_handoff(
    root: &Path,
    store: &Store,
    session: SessionId,
    objective: Option<&str>,
    suggested_skills: Vec<String>,
) -> Result<HandoffSummary, HarnessError> {
    let progress = read_optional(&root.join("PROGRESS.md"))
        .map(|text| Progress::parse(&text))
        .transpose()?;
    let brief = read_optional(&root.join("brief.md"))
        .map(|text| Brief::parse(&text))
        .transpose()?;

    let state = repo_state(root);
    let repo = brief
        .as_ref()
        .map(|b| b.name.clone())
        .or_else(|| root.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "workspace".to_string());

    let objective = objective
        .map(str::to_string)
        .or_else(|| brief.as_ref().map(|b| first_line(&b.summary)))
        .or_else(|| progress.as_ref().map(|p| p.name.clone()))
        .unwrap_or_else(|| "continue the work".to_string());

    let next_action = progress
        .as_ref()
        .and_then(|p| p.next_incomplete())
        .map(|step| format!("{}. {}", step.number, step.description))
        .unwrap_or_else(|| "no incomplete plan step recorded".to_string());

    // References: harness documents that actually exist, by path (never copied).
    let references: Vec<String> = ["brief.md", "PROGRESS.md", "DECISIONS.md"]
        .into_iter()
        .filter(|name| root.join(name).is_file())
        .map(str::to_string)
        .collect();

    let committed_steps = committed_steps(store, session);
    let (completed, total) = progress
        .as_ref()
        .map(|p| (p.completed_count(), p.steps.len()))
        .unwrap_or((0, 0));

    let id = new_handoff_id();
    let header = HandoffHeader {
        schema: HANDOFF_SCHEMA.to_string(),
        id: id.clone(),
        created: today(),
        repo,
        branch: state
            .branch
            .clone()
            .unwrap_or_else(|| "(unknown)".to_string()),
        commit: state
            .commit
            .clone()
            .unwrap_or_else(|| "(unknown)".to_string()),
        dirty: state.dirty,
        session: Some(session.to_string()),
        // Heuristic, not a promise: a handoff is a starting point to re-verify.
        confidence: 0.6,
        objective: objective.clone(),
        next_action: next_action.clone(),
        references: references.clone(),
        suggested_skills,
    };
    let body = render_body(&header, completed, total, &committed_steps);
    let artifact = Handoff { header, body };

    // Redact the whole artifact through the canonical host redactor before write.
    let redacted = redact(&artifact.render());
    let dir = root.join(".localpilot").join(HANDOFF_DIR);
    std::fs::create_dir_all(&dir).map_err(|source| HarnessError::Io {
        path: dir.display().to_string(),
        source,
    })?;
    let path = dir.join(format!("{id}.md"));
    localpilot_store::atomic_write(&path, redacted.as_bytes())
        .map_err(|e| HarnessError::Provider(format!("failed to write handoff: {e}")))?;
    Ok(HandoffSummary { id, path })
}

/// Build the human-readable body, separating confirmed facts from assumptions and
/// referencing the harness documents rather than duplicating them.
fn render_body(
    header: &HandoffHeader,
    completed: usize,
    total: usize,
    committed_steps: &[String],
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "# Handoff: {}\n", header.objective);

    let _ = writeln!(out, "## Confirmed facts\n");
    let dirty = if header.dirty { "dirty" } else { "clean" };
    let _ = writeln!(
        out,
        "- Repo `{}` on branch `{}` at commit `{}` ({dirty}).",
        header.repo, header.branch, header.commit
    );
    if total > 0 {
        let _ = writeln!(out, "- {completed} of {total} plan steps complete.");
    }
    for step in committed_steps {
        let _ = writeln!(out, "- {step}");
    }

    let _ = writeln!(out, "\n## Assumptions\n");
    let _ = writeln!(out, "- Next action: {} (not yet done).", header.next_action);
    let _ = writeln!(
        out,
        "- The objective is inferred from the harness documents; confirm it still holds."
    );

    if !header.references.is_empty() {
        let _ = writeln!(out, "\n## References (read, do not assume)\n");
        for reference in &header.references {
            let _ = writeln!(
                out,
                "- `{reference}` — authoritative; read it rather than trusting this summary."
            );
        }
    }

    if !header.suggested_skills.is_empty() {
        let _ = writeln!(out, "\n## Suggested skills\n");
        for skill in &header.suggested_skills {
            let _ = writeln!(out, "- `{skill}`");
        }
    }

    let _ = writeln!(out, "\n## Staleness\n");
    let _ = writeln!(
        out,
        "This handoff reflects the repo at commit `{}`. If HEAD has moved or the tree changed, treat \
         its facts as a flag to re-verify, not as truth — run `localpilot handoff resume {}` first.",
        header.commit, header.id
    );
    out
}

/// One confirmed-fact line per committed step in the session event log.
fn committed_steps(store: &Store, session: SessionId) -> Vec<String> {
    let Ok(events) = store.read_events(session) else {
        return Vec::new();
    };
    events
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::StepCompleted {
                number,
                commit: Some(commit),
                ..
            } => Some(format!("Step {number} committed as `{commit}`.")),
            _ => None,
        })
        .collect()
}

/// Current repo branch / commit / dirty state, best-effort (a non-git workspace
/// yields `None` for branch and commit and a clean tree).
#[derive(Debug, Clone, Default)]
struct RepoState {
    branch: Option<String>,
    commit: Option<String>,
    dirty: bool,
}

fn repo_state(root: &Path) -> RepoState {
    RepoState {
        branch: git(root, &["rev-parse", "--abbrev-ref", "HEAD"]),
        commit: git(root, &["rev-parse", "--short", "HEAD"]),
        dirty: git(root, &["status", "--porcelain"]).is_some_and(|s| !s.trim().is_empty()),
    }
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
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

/// A short, filename-safe, collision-resistant handoff id.
fn new_handoff_id() -> String {
    let raw = localpilot_core::EventId::new().as_uuid();
    let simple = raw.as_simple().to_string();
    format!("h-{}", &simple[..simple.len().min(12)])
}

// --- resume check --------------------------------------------------------------

/// The live repo facts the deterministic resume check compares the handoff header
/// against. Gathered separately from the comparison so the comparison is a pure,
/// unit-testable function.
#[derive(Debug, Clone)]
pub struct ResumeEnv {
    pub current_branch: Option<String>,
    pub head_commit: Option<String>,
    /// Whether the handoff's recorded commit still exists in the repo.
    pub commit_exists: bool,
    pub dirty: bool,
    /// Referenced paths from the header that are missing from the working tree.
    pub missing_references: Vec<String>,
    /// Whether the referenced session still exists in the store.
    pub session_exists: bool,
}

/// One resume-check finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeFinding {
    pub check: String,
    pub ok: bool,
    pub detail: String,
}

/// A resume-check report. `passed` is true only when every finding is OK.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeReport {
    pub findings: Vec<ResumeFinding>,
}

impl ResumeReport {
    /// Whether every check matched (no stale-state warning).
    #[must_use]
    pub fn passed(&self) -> bool {
        self.findings.iter().all(|f| f.ok)
    }

    /// Render the report as deterministic text.
    #[must_use]
    pub fn render(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        for finding in &self.findings {
            let mark = if finding.ok { "ok" } else { "warn" };
            let _ = writeln!(out, "[{mark}] {}: {}", finding.check, finding.detail);
        }
        let _ = writeln!(
            out,
            "\n{}",
            if self.passed() {
                "handoff matches the current repo — safe to resume."
            } else {
                "handoff may be stale — review the warnings before acting on it."
            }
        );
        out
    }
}

/// Compare a handoff header against the live repo facts. Pure and deterministic:
/// every mismatch is a warning (a flag to re-verify), never a hard failure, and no
/// model judges the prose body.
#[must_use]
pub fn evaluate_resume(header: &HandoffHeader, env: &ResumeEnv) -> ResumeReport {
    let mut findings = Vec::new();

    let branch_ok = env.current_branch.as_deref() == Some(header.branch.as_str());
    findings.push(ResumeFinding {
        check: "branch".to_string(),
        ok: branch_ok,
        detail: if branch_ok {
            format!("on `{}`", header.branch)
        } else {
            format!(
                "handoff says `{}`, repo is on `{}`",
                header.branch,
                env.current_branch.as_deref().unwrap_or("(none)")
            )
        },
    });

    findings.push(ResumeFinding {
        check: "commit".to_string(),
        ok: env.commit_exists,
        detail: if env.commit_exists {
            let moved = env.head_commit.as_deref() != Some(header.commit.as_str());
            if moved {
                format!(
                    "commit `{}` exists but HEAD is now `{}` — history moved",
                    header.commit,
                    env.head_commit.as_deref().unwrap_or("(unknown)")
                )
            } else {
                format!("HEAD is `{}`", header.commit)
            }
        } else {
            format!(
                "commit `{}` not found in this repo — may be stale",
                header.commit
            )
        },
    });

    let dirty_ok = env.dirty == header.dirty;
    findings.push(ResumeFinding {
        check: "tree".to_string(),
        ok: dirty_ok,
        detail: if dirty_ok {
            format!("tree is {}", if env.dirty { "dirty" } else { "clean" })
        } else {
            format!(
                "handoff recorded a {} tree, repo is now {}",
                if header.dirty { "dirty" } else { "clean" },
                if env.dirty { "dirty" } else { "clean" }
            )
        },
    });

    let refs_ok = env.missing_references.is_empty();
    findings.push(ResumeFinding {
        check: "references".to_string(),
        ok: refs_ok,
        detail: if refs_ok {
            "all referenced documents are present".to_string()
        } else {
            format!("missing: {}", env.missing_references.join(", "))
        },
    });

    if header.session.is_some() {
        findings.push(ResumeFinding {
            check: "session".to_string(),
            ok: env.session_exists,
            detail: if env.session_exists {
                "referenced session is present".to_string()
            } else {
                "referenced session is missing from the store".to_string()
            },
        });
    }

    ResumeReport { findings }
}

/// Load a handoff by id and run the deterministic resume check against `root`.
///
/// # Errors
/// Returns [`HarnessError`] if the handoff file is missing or malformed.
pub fn check_handoff(
    root: &Path,
    store: &Store,
    id: &str,
) -> Result<(Handoff, ResumeReport), HarnessError> {
    let path = root
        .join(".localpilot")
        .join(HANDOFF_DIR)
        .join(format!("{id}.md"));
    let text = std::fs::read_to_string(&path).map_err(|source| HarnessError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let handoff = Handoff::parse(&text)?;
    let state = repo_state(root);
    let commit_exists = git(
        root,
        &[
            "cat-file",
            "-e",
            &format!("{}^{{commit}}", handoff.header.commit),
        ],
    )
    .is_some()
        || git(
            root,
            &["rev-parse", "--verify", "--quiet", &handoff.header.commit],
        )
        .is_some();
    let missing_references: Vec<String> = handoff
        .header
        .references
        .iter()
        .filter(|name| !root.join(name).exists())
        .cloned()
        .collect();
    let session_exists = handoff
        .header
        .session
        .as_deref()
        .and_then(|s| s.parse::<SessionId>().ok())
        .is_some_and(|sid| {
            store
                .read_events(sid)
                .map(|e| !e.is_empty())
                .unwrap_or(false)
        });
    let env = ResumeEnv {
        current_branch: state.branch,
        head_commit: state.commit,
        commit_exists,
        dirty: state.dirty,
        missing_references,
        session_exists,
    };
    let report = evaluate_resume(&handoff.header, &env);
    Ok((handoff, report))
}

fn read_optional(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use localpilot_core::{Message, Role};

    fn sample_header() -> HandoffHeader {
        HandoffHeader {
            schema: HANDOFF_SCHEMA.to_string(),
            id: "h-abc123".to_string(),
            created: "2026-06-17".to_string(),
            repo: "demo".to_string(),
            branch: "main".to_string(),
            commit: "abc1234".to_string(),
            dirty: false,
            session: Some(SessionId::new().to_string()),
            confidence: 0.6,
            objective: "ship the parser".to_string(),
            next_action: "2. wire the loader".to_string(),
            references: vec!["PROGRESS.md".to_string(), "DECISIONS.md".to_string()],
            suggested_skills: vec!["add-provider".to_string()],
        }
    }

    #[test]
    fn header_round_trips_through_render_and_parse() {
        let header = sample_header();
        let artifact = Handoff {
            header: header.clone(),
            body: "# Handoff: ship the parser\n\nbody text\n".to_string(),
        };
        let parsed = Handoff::parse(&artifact.render()).unwrap();
        assert_eq!(parsed.header, header);
        assert!(parsed.body.contains("body text"));
    }

    #[test]
    fn write_references_documents_by_path_without_duplicating_them() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("PROGRESS.md"),
            "# Progress: demo\nBranch: main\n\n## Steps\n\n- [x] 1. first\n- [ ] 2. second\n",
        )
        .unwrap();
        std::fs::write(
            root.join("DECISIONS.md"),
            "# Decisions: demo\n\n- D001 · 2026-06-17 · a\n  - decision: x\n  - rationale: y\n  - refs: z\n",
        )
        .unwrap();
        let store = Store::open(root);
        let session = SessionId::new();
        store
            .append_message(session, &Message::text(Role::User, "hi"))
            .unwrap();

        let summary = write_handoff(root, &store, session, None, Vec::new()).unwrap();
        let text = std::fs::read_to_string(&summary.path).unwrap();

        // References the documents by path...
        assert!(text.contains("PROGRESS.md"), "{text}");
        assert!(text.contains("DECISIONS.md"), "{text}");
        // ...rather than copying their bodies in.
        assert!(
            !text.contains("rationale: y"),
            "decision body was duplicated: {text}"
        );
        // The next action comes from the next incomplete step.
        assert!(text.contains("second"), "{text}");
        // Stored under the git-ignored handoffs dir.
        assert!(summary
            .path
            .starts_with(root.join(".localpilot").join("handoffs")));
    }

    #[test]
    fn a_planted_secret_never_reaches_the_written_handoff() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Plant a secret in the next plan step, which flows into next_action.
        std::fs::write(
            root.join("PROGRESS.md"),
            "# Progress: demo\nBranch: main\n\n## Steps\n\n- [ ] 1. use sk-abcdefghijklmnopqrstuvwxyz0123 to call the api\n",
        )
        .unwrap();
        let store = Store::open(root);
        let session = SessionId::new();
        store
            .append_message(session, &Message::text(Role::User, "hi"))
            .unwrap();

        let summary = write_handoff(root, &store, session, None, Vec::new()).unwrap();
        let text = std::fs::read_to_string(&summary.path).unwrap();
        assert!(
            !text.contains("sk-abcdefghijklmnopqrstuvwxyz0123"),
            "secret leaked: {text}"
        );
        assert!(
            text.contains("[REDACTED]"),
            "expected redaction marker: {text}"
        );
    }

    #[test]
    fn resume_clean_match_passes_and_a_stale_commit_warns() {
        let header = sample_header();

        // A clean match: same branch, the commit exists and is HEAD, tree clean,
        // refs present, session present.
        let clean = ResumeEnv {
            current_branch: Some("main".to_string()),
            head_commit: Some("abc1234".to_string()),
            commit_exists: true,
            dirty: false,
            missing_references: Vec::new(),
            session_exists: true,
        };
        let report = evaluate_resume(&header, &clean);
        assert!(report.passed(), "{}", report.render());

        // A stale commit: the recorded commit no longer exists.
        let stale = ResumeEnv {
            commit_exists: false,
            head_commit: Some("def5678".to_string()),
            ..clean.clone()
        };
        let report = evaluate_resume(&header, &stale);
        assert!(!report.passed());
        let commit = report
            .findings
            .iter()
            .find(|f| f.check == "commit")
            .unwrap();
        assert!(!commit.ok);
        assert!(commit.detail.contains("stale"), "{}", commit.detail);
    }

    #[test]
    fn resume_warns_on_a_branch_mismatch() {
        let header = sample_header();
        let env = ResumeEnv {
            current_branch: Some("feature/x".to_string()),
            head_commit: Some("abc1234".to_string()),
            commit_exists: true,
            dirty: false,
            missing_references: Vec::new(),
            session_exists: true,
        };
        let report = evaluate_resume(&header, &env);
        assert!(!report.passed());
        let branch = report
            .findings
            .iter()
            .find(|f| f.check == "branch")
            .unwrap();
        assert!(!branch.ok);
    }
}
