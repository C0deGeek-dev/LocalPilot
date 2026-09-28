//! The participant engine's protocol half: what to do with each delivery,
//! decided in code. A model is asked only for judgement, through a
//! [`Request`], and its answer is accepted only after [`validate`] has turned
//! it into a post the protocol allows.
//!
//! The loop around it (receive, plan, judge, post, acknowledge) lives with
//! the model runtime; everything here is deterministic and model-free.
//!
//! - [`Mesh::plan`] maps one delivery to [`Step`]s.
//! - A review request's fingerprint manifest is checked here, before any
//!   model is involved; a request that fails it gets a `REVISE` from the
//!   engine itself.
//! - [`parse_answer`] and [`validate`] accept a model's structured answer, or
//!   say why not; [`escalation`] is what is posted after a second failure.
//! - [`Mesh::replied`] finds a reply already posted, so a redelivered message
//!   is acknowledged rather than answered twice.

use std::path::{Component, Path};

use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::post::{Expect, PostArgs};
use super::read::Delivery;
use super::unit::git;
use super::{authority, schema, sid, str_of, Mesh, Obj, MAX_BODY};
use crate::error::MeshError;
use crate::jsonl;
use crate::layout::MAILBOX_DIR;

/// The active session as `doctor` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    pub schema: i64,
    pub participants: Vec<String>,
    pub delivery: String,
}

/// What a model is asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// A `VERDICT` on a review request.
    Review,
    /// A reply to a message that expects one.
    Reply,
}

/// One judgement the engine needs from a model.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub need: Need,
    /// The message being answered, e.g. `claude:12`.
    pub msg_id: String,
    pub kind: String,
    pub from: String,
    pub body: String,
    /// This participant's verdict round in the unit, for the header.
    pub round: i64,
    /// For a review: the verified manifest, anchor-relative paths.
    pub files: Vec<String>,
    pub expect: Expect,
}

/// What the loop does next.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Ask the model, validate, post under `expect`, then acknowledge
    /// `request.msg_id`.
    Judge(Request),
    /// Post this, then acknowledge `ack`.
    Post {
        args: PostArgs,
        expect: Expect,
        ack: String,
    },
    /// Acknowledge without posting: nothing is owed, it was already
    /// answered, or it went stale.
    Ack { msg_id: String, why: String },
    /// A notice about a peer; nothing to acknowledge.
    Notice(String),
    /// A `STOP`: acknowledge it, then stop the loop.
    Stop { msg_id: String, reason: String },
}

/// The kinds a reply may take.
const REPLY_KINDS: &[&str] = &[
    "ANSWER",
    "CHALLENGE",
    "DESIGN_AGREED",
    "QUESTION",
    "ESCALATE",
];

impl Mesh {
    /// The steps for one delivery to `role`.
    ///
    /// # Errors
    /// A refusal reading the session, or a session that is not schema 2
    /// (the engine replies by `msg_id`, which only schema 2 has).
    pub fn plan(&self, role: &str, delivery: &Delivery) -> Result<Vec<Step>, MeshError> {
        let messages = match delivery {
            Delivery::Notice(line) => return Ok(vec![Step::Notice(line.clone())]),
            Delivery::Mail { messages, .. } => messages,
        };
        let s = self.require(role, true)?;
        if schema(&s) != 2 {
            return Err(super::refused(
                "the participant engine needs an N-party (schema 2) session; start it with --with",
            ));
        }
        let mut steps = Vec::new();
        for d in messages {
            steps.push(self.step_for(&s, role, &d.record)?);
        }
        Ok(steps)
    }

    fn step_for(&self, s: &Obj, role: &str, m: &Obj) -> Result<Step, MeshError> {
        let msg_id = str_of(m, "msg_id").unwrap_or_default().to_owned();
        let kind = str_of(m, "kind").unwrap_or_default().to_owned();
        let ack = |why: &str| Step::Ack {
            msg_id: msg_id.clone(),
            why: why.to_owned(),
        };
        if self.replied(role, &msg_id)? {
            return Ok(ack("already answered"));
        }
        if kind == "STOP" {
            let reason = str_of(m, "body")
                .and_then(|b| b.lines().next())
                .unwrap_or_default()
                .to_owned();
            return Ok(Step::Stop { msg_id, reason });
        }
        let unit = str_of(m, "unit_id").map(str::to_owned);
        if unit.as_deref() != str_of(s, "unit_id") {
            return Ok(ack("from an earlier unit"));
        }
        let expects_reply = m
            .get("expect_reply")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let from = str_of(m, "role").unwrap_or_default().to_owned();
        let body = str_of(m, "body").unwrap_or_default().to_owned();
        let expect = |reviewer: bool| Expect {
            session_id: sid(s).to_owned(),
            unit_id: unit.clone(),
            reviewer,
        };
        if kind == "REVIEW_REQUEST" {
            let (owner, required, _) = authority(s);
            if owner == role || !required.iter().any(|r| r == role) {
                return Ok(ack("not a required reviewer"));
            }
            let round = self.verdicts_in_unit(s, role)? + 1;
            return Ok(match self.check_manifest(s, &body)? {
                Err(problems) => Step::Post {
                    args: revise_for(&problems, round, &msg_id),
                    expect: expect(true),
                    ack: msg_id,
                },
                Ok(files) => Step::Judge(Request {
                    need: Need::Review,
                    msg_id,
                    kind,
                    from,
                    body,
                    round,
                    files,
                    expect: expect(true),
                }),
            });
        }
        if !expects_reply {
            return Ok(ack("no reply owed"));
        }
        if kind == "HANDOFF_OFFER" {
            return Ok(Step::Post {
                args: PostArgs {
                    kind: "NOTE".into(),
                    body: "localpilot does not take ownership in this build; the offer stays \
                           open for another participant or the user."
                        .into(),
                    reply_to: Some(msg_id.clone()),
                    ..PostArgs::default()
                },
                expect: expect(false),
                ack: msg_id,
            });
        }
        Ok(Step::Judge(Request {
            need: Need::Reply,
            msg_id,
            kind,
            from,
            body,
            round: 0,
            files: Vec::new(),
            expect: expect(false),
        }))
    }

    /// The active session, read and never written: `None` when there is
    /// none.
    ///
    /// # Errors
    /// An unreadable, corrupt or refused pointer or session record, kept
    /// apart from "no session" so a caller can say which.
    pub fn session_summary(&self) -> Result<Option<SessionSummary>, MeshError> {
        Ok(self.active()?.map(|s| SessionSummary {
            id: sid(&s).to_owned(),
            schema: schema(&s),
            participants: super::participants(&s),
            delivery: super::delivery(&s).to_owned(),
        }))
    }

    /// The active session's id, if the participant engine can run in it as
    /// `role`: a schema-2 session under acknowledged delivery, so every
    /// answer can name what it replies to and nothing is lost before it is
    /// handled.
    ///
    /// # Errors
    /// A refusal naming what is missing.
    pub fn engine_ready(&self, role: &str) -> Result<String, MeshError> {
        let s = self.require(role, true)?;
        if schema(&s) != 2 {
            return Err(super::refused(
                "the participant engine needs an N-party (schema 2) session; start it with --with",
            ));
        }
        if super::delivery(&s) != "ack" {
            return Err(super::refused(
                "the participant engine needs acknowledged delivery; every participant must join with an ack-capable build",
            ));
        }
        Ok(sid(&s).to_owned())
    }

    /// A review request's manifest checked again, just before its verdict is
    /// posted: `None` while it still holds, or the engine's own `REVISE` when
    /// the tree changed during the review.
    ///
    /// # Errors
    /// A refusal reading the session, or a tree that cannot be observed.
    pub fn recheck_review(
        &self,
        role: &str,
        request: &Request,
    ) -> Result<Option<PostArgs>, MeshError> {
        let s = self.require(role, true)?;
        Ok(match self.check_manifest(&s, &request.body)? {
            Ok(_) => None,
            Err(problems) => Some(revise_for(&problems, request.round, &request.msg_id)),
        })
    }

    /// Whether `role` has already posted a reply to `msg_id`.
    ///
    /// # Errors
    /// Reading the journal failed.
    pub fn replied(&self, role: &str, msg_id: &str) -> Result<bool, MeshError> {
        let Some(s) = self.active()? else {
            return Ok(false);
        };
        Ok(jsonl::records(&self.mb.journal(sid(&s), role))?
            .iter()
            .any(|r| str_of(r, "reply_to") == Some(msg_id)))
    }

    fn verdicts_in_unit(&self, s: &Obj, role: &str) -> Result<i64, MeshError> {
        let unit = str_of(s, "unit_id");
        let n = jsonl::records(&self.mb.journal(sid(s), role))?
            .iter()
            .filter(|r| str_of(r, "kind") == Some("VERDICT") && str_of(r, "unit_id") == unit)
            .count();
        Ok(i64::try_from(n).unwrap_or(i64::MAX))
    }

    /// The manifest of a review request, verified against the tree: the
    /// listed files, or every problem found.
    fn check_manifest(
        &self,
        s: &Obj,
        body: &str,
    ) -> Result<Result<Vec<String>, Vec<String>>, MeshError> {
        let manifest = match parse_manifest(body) {
            Ok(m) => m,
            Err(problems) => return Ok(Err(problems)),
        };
        let root = self.repo();
        let no_vcs = self.no_vcs(s)?;
        let changed = if no_vcs {
            Vec::new()
        } else {
            changed_paths(root)?
        };
        let mut problems = Vec::new();
        for (path, want) in &manifest {
            match fingerprint_of(root, path) {
                Err(why) => problems.push(format!("{path}: {why}")),
                // A deletion counts only when the tree shows it: a manifest of
                // made-up deleted paths would otherwise review nothing.
                Ok(None) if want == "deleted" && no_vcs => problems.push(format!(
                    "{path}: a deletion cannot be verified without version control; describe it in the request instead"
                )),
                Ok(None) if want == "deleted" && !changed.contains(path) => {
                    problems.push(format!("{path}: listed as deleted, but Git shows no such deletion"));
                }
                Ok(None) if want == "deleted" => {}
                Ok(None) => problems.push(format!("{path}: missing, but listed as {want}")),
                Ok(Some(got)) if want == "deleted" => {
                    problems.push(format!("{path}: listed as deleted, but present ({got})"));
                }
                Ok(Some(got)) if &got != want => {
                    problems.push(format!(
                        "{path}: fingerprint {got}, but the request says {want}"
                    ));
                }
                Ok(Some(_)) => {}
            }
        }
        // Without version control there is no list of changed files to hold
        // the manifest to; only its entries are checked.
        for path in &changed {
            if !manifest.iter().any(|(p, _)| p == path) {
                problems.push(format!("{path}: changed, but not in the manifest"));
            }
        }
        if problems.is_empty() {
            Ok(Ok(manifest.into_iter().map(|(p, _)| p).collect()))
        } else {
            Ok(Err(problems))
        }
    }
}

/// The fingerprint every participant uses: SHA-256 of the file's bytes with
/// each CRLF read as LF (a lone CR stays), first 12 hex digits. `None` when
/// the file
/// does not exist.
///
/// # Errors
/// A path that is not a plain anchor-relative path, resolves outside the
/// anchor, or cannot be read.
pub fn fingerprint_of(root: &Path, rel: &str) -> Result<Option<String>, String> {
    let p = Path::new(rel);
    if p.is_absolute()
        || rel.contains('\\')
        || p.components().any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err("not a plain anchor-relative path".into());
    }
    if p.components()
        .next()
        .is_some_and(|c| c.as_os_str() == MAILBOX_DIR)
    {
        return Err("inside the mailbox".into());
    }
    let full = root.join(p);
    if !full.exists() && full.symlink_metadata().is_err() {
        return Ok(None);
    }
    let canon_root = root.canonicalize().map_err(|e| e.to_string())?;
    let canon = full.canonicalize().map_err(|e| e.to_string())?;
    if !canon.starts_with(&canon_root) {
        return Err("resolves outside the anchor".into());
    }
    let bytes = std::fs::read(&canon).map_err(|e| e.to_string())?;
    Ok(Some(crate::tree::hex(
        &Sha256::digest(crlf_to_lf(&bytes))[..6],
    )))
}

/// `bytes` with every CRLF pair replaced by LF; any other CR is kept.
fn crlf_to_lf(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut it = bytes.iter().peekable();
    while let Some(&b) = it.next() {
        if b == b'\r' && it.peek() == Some(&&b'\n') {
            continue;
        }
        out.push(b);
    }
    out
}

/// The manifest in a review request: after the first line that starts with
/// `Fingerprints`, every line of the form `path=<12 hex>` or `path=deleted`.
/// Blank lines end nothing; any other non-blank line after the header is
/// malformed.
///
/// # Errors
/// Every problem: no header, no entries, a malformed line, a duplicate.
pub fn parse_manifest(body: &str) -> Result<Vec<(String, String)>, Vec<String>> {
    let mut lines = body.lines();
    if !lines.any(|l| l.trim_start().starts_with("Fingerprints")) {
        return Err(vec!["no Fingerprints manifest in the request".into()]);
    }
    let mut out: Vec<(String, String)> = Vec::new();
    let mut problems = Vec::new();
    for line in lines {
        let line = line.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        let parsed = line.rsplit_once('=').filter(|(path, value)| {
            !path.is_empty()
                && !path.starts_with(char::is_whitespace)
                && (*value == "deleted"
                    || (value.len() == 12
                        && value
                            .bytes()
                            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())))
        });
        match parsed {
            Some((path, _)) if out.iter().any(|(p, _)| p == path) => {
                problems.push(format!("{path}: listed twice"));
            }
            Some((path, value)) => out.push((path.to_owned(), value.to_owned())),
            None => problems.push(format!("malformed manifest line: {line}")),
        }
    }
    if out.is_empty() && problems.is_empty() {
        problems.push("the Fingerprints manifest is empty".into());
    }
    if problems.is_empty() {
        Ok(out)
    } else {
        Err(problems)
    }
}

/// Every changed or untracked path under the anchor, anchor-relative, the
/// mailbox excluded.
fn changed_paths(root: &Path) -> Result<Vec<String>, MeshError> {
    let (rc, prefix) = git(root, &["rev-parse", "--show-prefix"])?;
    if rc != Some(0) {
        return Err(super::refused(
            "cannot observe the working tree: git rev-parse failed",
        ));
    }
    let (rc, out) = git(
        root,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--",
            ".",
        ],
    )?;
    if rc != Some(0) {
        return Err(super::refused(
            "cannot observe the working tree: git status failed",
        ));
    }
    let mut paths = Vec::new();
    let mut fields = out.split('\0').filter(|f| !f.is_empty());
    while let Some(entry) = fields.next() {
        let (xy, path) = entry.split_at(entry.len().min(3));
        if xy.starts_with('R') || xy.starts_with('C') {
            let _source = fields.next();
        }
        let rel = path.strip_prefix(prefix.trim()).unwrap_or(path);
        if !rel.starts_with(&format!("{MAILBOX_DIR}/")) {
            paths.push(rel.to_owned());
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn revise_for(problems: &[String], round: i64, msg_id: &str) -> PostArgs {
    let mut body = format!(
        "REVISE round={round} blocking={} important=0\nThe request's fingerprint manifest does not match the tree, so it was not reviewed:",
        problems.len()
    );
    // Room for the closing line, whatever the count.
    let budget = MAX_BODY - 80;
    for (i, p) in problems.iter().enumerate() {
        let line: String = p.chars().take(300).collect();
        if body.chars().count() + line.chars().count() + 3 > budget {
            body.push_str(&format!("\n- ... and {} more", problems.len() - i));
            break;
        }
        body.push_str("\n- ");
        body.push_str(&line);
    }
    PostArgs {
        kind: "VERDICT".into(),
        body,
        reply_to: Some(msg_id.to_owned()),
        ..PostArgs::default()
    }
}

/// A finding in a verdict.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub file: String,
    #[serde(default)]
    pub line: Option<u32>,
    pub severity: Severity,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Blocking,
    Important,
    Minor,
}

/// A model's structured answer.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Answer {
    pub kind: String,
    /// For a verdict: `AGREE` or `REVISE`.
    #[serde(default)]
    pub decision: Option<String>,
    #[serde(default)]
    pub findings: Vec<Finding>,
    pub body: String,
}

/// The last JSON object in a model's final text that parses as an
/// [`Answer`].
///
/// # Errors
/// No such object; the error says what the last candidate lacked.
pub fn parse_answer(text: &str) -> Result<Answer, String> {
    let mut last_error = "no JSON object in the reply".to_owned();
    for (i, _) in text.char_indices().rev().filter(|(_, c)| *c == '{') {
        let mut stream = serde_json::Deserializer::from_str(&text[i..]).into_iter::<Value>();
        let Some(Ok(value)) = stream.next() else {
            continue;
        };
        if !value.is_object() {
            continue;
        }
        match serde_json::from_value::<Answer>(value) {
            Ok(answer) => return Ok(answer),
            Err(e) => last_error = format!("the JSON object is not a valid answer: {e}"),
        }
    }
    Err(last_error)
}

/// Turn a model's answer to `request` into the post the engine will make.
///
/// # Errors
/// Why the answer cannot be posted; it is fed back to the model once.
pub fn validate(request: &Request, answer: &Answer) -> Result<PostArgs, String> {
    let text = answer.body.trim();
    if text.is_empty() {
        return Err("the body is empty".into());
    }
    let body = match request.need {
        Need::Review => {
            if answer.kind != "VERDICT" {
                return Err(format!(
                    "a review is answered with VERDICT, not {}",
                    answer.kind
                ));
            }
            let blocking = count(&answer.findings, Severity::Blocking);
            let important = count(&answer.findings, Severity::Important);
            let decision = answer.decision.as_deref().unwrap_or_default();
            match decision {
                "AGREE" if blocking > 0 => {
                    return Err(
                        "AGREE with a blocking finding; decide REVISE or drop the finding".into(),
                    );
                }
                "REVISE" if answer.findings.is_empty() => {
                    return Err("REVISE needs at least one finding".into());
                }
                "AGREE" | "REVISE" => {}
                other => return Err(format!("decision must be AGREE or REVISE, not {other:?}")),
            }
            let mut out = format!(
                "{decision} round={} blocking={blocking} important={important}",
                request.round
            );
            for f in &answer.findings {
                let at = f.line.map_or(String::new(), |l| format!(":{l}"));
                let sev = match f.severity {
                    Severity::Blocking => "blocking",
                    Severity::Important => "important",
                    Severity::Minor => "minor",
                };
                out.push_str(&format!("\n- {}{at} [{sev}] {}", f.file, f.text.trim()));
            }
            out.push('\n');
            out.push_str(text);
            out
        }
        Need::Reply => {
            if !REPLY_KINDS.contains(&answer.kind.as_str()) {
                return Err(format!(
                    "a reply is one of {}, not {}",
                    REPLY_KINDS.join(", "),
                    answer.kind
                ));
            }
            if answer.decision.is_some() || !answer.findings.is_empty() {
                return Err("decision and findings belong to a VERDICT only".into());
            }
            text.to_owned()
        }
    };
    if body.chars().count() > MAX_BODY {
        return Err(format!("the post would exceed {MAX_BODY} characters"));
    }
    let kind = if request.need == Need::Review {
        "VERDICT".to_owned()
    } else {
        answer.kind.clone()
    };
    Ok(PostArgs {
        expect_reply: matches!(kind.as_str(), "QUESTION" | "CHALLENGE"),
        kind,
        body,
        reply_to: Some(request.msg_id.clone()),
        ..PostArgs::default()
    })
}

fn count(findings: &[Finding], severity: Severity) -> usize {
    findings.iter().filter(|f| f.severity == severity).count()
}

/// What is posted when the model failed twice: never its text.
#[must_use]
pub fn escalation(request: &Request, error: &str) -> PostArgs {
    PostArgs {
        kind: "ESCALATE".into(),
        body: format!(
            "localpilot could not produce a valid {} for {}: {error}. Nothing was posted in its place; a person or another participant needs to answer it.",
            if request.need == Need::Review { "VERDICT" } else { "reply" },
            request.msg_id
        ),
        reply_to: Some(request.msg_id.clone()),
        ..PostArgs::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::read::Delivered;
    use serde_json::json;
    use std::path::PathBuf;
    use std::process::Command;

    const SID: &str = "20260101T000000Z-abcdef12";
    const UNIT: &str = "1-abc";

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    /// A Git repository holding a committed `a.txt`, with a schema-2 session
    /// owned by claude in which localpilot is the required reviewer.
    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.email", "t@example.invalid"]);
        git(&root, &["config", "user.name", "t"]);
        git(&root, &["config", "core.autocrlf", "false"]);
        std::fs::write(root.join("a.txt"), "alpha\n").unwrap();
        git(&root, &["add", "a.txt"]);
        git(&root, &["commit", "-qm", "base"]);
        std::fs::create_dir_all(root.join(".git").join("info")).unwrap();
        std::fs::write(
            root.join(".git").join("info").join("exclude"),
            "/.pair-programming/\n",
        )
        .unwrap();
        let sd = root.join(".pair-programming").join("sessions").join(SID);
        std::fs::create_dir_all(sd.join("journal")).unwrap();
        write_session(&root, &session_record());
        std::fs::write(
            root.join(".pair-programming").join("active.json"),
            format!(
                "{}{SID}: see active.v2.json.\n",
                crate::layout::SENTINEL_PREFIX
            ),
        )
        .unwrap();
        std::fs::write(
            root.join(".pair-programming").join("active.v2.json"),
            json!({"session_id": SID, "driver": "claude", "status": "active",
                   "updated_at": "2026-01-01T00:00:00Z", "participants": ["claude", "localpilot"], "schema": 2})
            .to_string(),
        )
        .unwrap();
        (dir, root)
    }

    fn session_record() -> Value {
        json!({"session_id": SID, "task": "t", "work_unit": "t", "unit_id": UNIT, "driver": "claude",
               "owner": "claude", "ownership_epoch": 1, "status": "active", "phase": "review",
               "waiting": null, "handoff": null, "vcs": "git", "companions": [], "delivery": "ack",
               "protocol": "1.0", "schema": 2, "participants": ["claude", "localpilot"],
               "authority": {"required_reviewers": ["localpilot"], "advisers": []}, "pauses": {}})
    }

    fn session_path(root: &Path) -> PathBuf {
        root.join(".pair-programming")
            .join("sessions")
            .join(SID)
            .join("session.v2.json")
    }

    fn write_session(root: &Path, rec: &Value) {
        std::fs::write(session_path(root), rec.to_string()).unwrap();
    }

    fn edit_session(root: &Path, key: &str, value: Value) {
        let mut s: Obj =
            serde_json::from_str(&std::fs::read_to_string(session_path(root)).unwrap()).unwrap();
        s.insert(key.into(), value);
        write_session(root, &Value::Object(s));
    }

    fn message(role: &str, seq: i64, kind: &str, body: &str, expect_reply: bool) -> Obj {
        json!({"seq": seq, "at": "2026-01-01T00:00:00Z", "role": role, "kind": kind, "work_unit": "t",
               "unit_id": UNIT, "expect_reply": expect_reply, "body": body, "owner": "claude",
               "msg_id": format!("{role}:{seq}"), "to": ["localpilot"], "reply_to": null,
               "broadcast": false, "thread_id": format!("{role}:{seq}"), "route_trace": [role],
               "ttl": null, "forward": false})
        .as_object()
        .unwrap()
        .clone()
    }

    fn mail(m: Obj) -> Delivery {
        Delivery::Mail {
            messages: vec![Delivered {
                record: m,
                count: 1,
            }],
            marks: false,
        }
    }

    fn mesh(root: &Path) -> Mesh {
        Mesh::at(root, "flag")
    }

    fn fp(root: &Path, rel: &str) -> String {
        fingerprint_of(root, rel).unwrap().unwrap()
    }

    fn review(body: &str) -> Delivery {
        mail(message("claude", 1, "REVIEW_REQUEST", body, false))
    }

    fn one(steps: Vec<Step>) -> Step {
        assert_eq!(steps.len(), 1, "{steps:?}");
        steps.into_iter().next().unwrap()
    }

    #[test]
    fn a_review_with_a_matching_manifest_goes_to_the_model() {
        let (_d, root) = fixture();
        std::fs::write(root.join("a.txt"), "changed\r\n").unwrap();
        let body = format!(
            "Please review.\n\nFingerprints:\na.txt={}\n",
            fp(&root, "a.txt")
        );
        let Step::Judge(req) = one(mesh(&root).plan("localpilot", &review(&body)).unwrap()) else {
            panic!("expected a judgement")
        };
        assert_eq!(req.need, Need::Review);
        assert_eq!(req.msg_id, "claude:1");
        assert_eq!(req.round, 1);
        assert_eq!(req.files, ["a.txt"]);
        assert_eq!(
            req.expect,
            Expect {
                session_id: SID.into(),
                unit_id: Some(UNIT.into()),
                reviewer: true
            }
        );
        // The fingerprint ignores carriage returns.
        assert_eq!(
            fp(&root, "a.txt"),
            crate::tree::hex(&Sha256::digest(b"changed\n")[..6])
        );
    }

    fn revise_body(root: &Path, body: &str) -> String {
        match one(mesh(root).plan("localpilot", &review(body)).unwrap()) {
            Step::Post { args, expect, ack } => {
                assert_eq!(args.kind, "VERDICT");
                assert_eq!(args.reply_to.as_deref(), Some("claude:1"));
                assert_eq!(ack, "claude:1");
                assert!(expect.reviewer);
                assert!(
                    args.body.starts_with("REVISE round=1 blocking="),
                    "{}",
                    args.body
                );
                args.body
            }
            other => panic!("expected the engine's own REVISE, got {other:?}"),
        }
    }

    #[test]
    fn a_manifest_that_does_not_hold_is_revised_without_a_model() {
        // Bug it prevents: an empty or wrong manifest reaching a model-backed
        // AGREE.
        let (_d, root) = fixture();
        std::fs::write(root.join("a.txt"), "changed\n").unwrap();
        let good = fp(&root, "a.txt");
        let cases: Vec<(String, &str)> = vec![
            ("no manifest at all".into(), "no Fingerprints manifest"),
            ("Fingerprints:\n".into(), "manifest is empty"),
            (
                format!("Fingerprints:\na.txt={good}\nnot a manifest line"),
                "malformed manifest line",
            ),
            (
                "Fingerprints:\na.txt=000000000000\n".into(),
                "a.txt: fingerprint",
            ),
            (
                format!("Fingerprints:\na.txt={good}\na.txt={good}\n"),
                "listed twice",
            ),
            (
                format!("Fingerprints:\na.txt={good}\n../x=000000000000\n"),
                "../x: not a plain",
            ),
            (
                format!("Fingerprints:\na.txt={good}\n.pair-programming/x=000000000000\n"),
                "inside the mailbox",
            ),
            (
                format!("Fingerprints:\na.txt={good}\nmissing.txt=000000000000\n"),
                "missing.txt: missing",
            ),
            (
                format!("Fingerprints:\na.txt={}\n", good.to_uppercase()),
                "malformed manifest line",
            ),
        ];
        for (body, why) in cases {
            let text = revise_body(&root, &body);
            assert!(text.contains(why), "{body:?}: {text}");
        }
        // A changed file the manifest leaves out.
        std::fs::write(root.join("b.txt"), "new\n").unwrap();
        let text = revise_body(&root, &format!("Fingerprints:\na.txt={good}\n"));
        assert!(
            text.contains("b.txt: changed, but not in the manifest"),
            "{text}"
        );
        // A deletion is listed as `deleted`, and must be a deletion Git
        // shows: a made-up deleted path reviews nothing.
        std::fs::remove_file(root.join("b.txt")).unwrap();
        std::fs::write(root.join("a.txt"), "alpha\n").unwrap();
        let text = revise_body(&root, "Fingerprints:\nnever-existed.txt=deleted\n");
        assert!(text.contains("Git shows no such deletion"), "{text}");
        std::fs::remove_file(root.join("a.txt")).unwrap();
        assert!(matches!(
            one(mesh(&root)
                .plan("localpilot", &review("Fingerprints:\na.txt=deleted\n"))
                .unwrap()),
            Step::Judge(_)
        ));
        std::fs::write(root.join("a.txt"), "back\n").unwrap();
        let text = revise_body(&root, "Fingerprints:\na.txt=deleted\n");
        assert!(text.contains("listed as deleted, but present"), "{text}");
    }

    #[test]
    fn the_fingerprint_reads_crlf_as_lf_and_keeps_a_lone_cr() {
        let dir = tempfile::tempdir().unwrap();
        let hash = |bytes: &[u8]| {
            std::fs::write(dir.path().join("f"), bytes).unwrap();
            fingerprint_of(dir.path(), "f").unwrap().unwrap()
        };
        assert_eq!(hash(b"a\r\nb\r\n"), hash(b"a\nb\n"));
        assert_ne!(hash(b"a\rb"), hash(b"ab"));
        assert_ne!(hash(b"a\r\r\nb"), hash(b"a\nb"));
        // One pass: the CR before a CRLF is a lone CR, and it stays.
        assert_eq!(
            hash(b"a\r\r\nb"),
            crate::tree::hex(&Sha256::digest(b"a\r\nb")[..6])
        );
        assert_eq!(hash(b"x\n"), crate::tree::hex(&Sha256::digest(b"x\n")[..6]));
    }

    #[test]
    fn a_huge_bad_manifest_still_gets_a_postable_revise() {
        // Bug it prevents: a REVISE longer than a post may be, refused by
        // post, so the request is never acknowledged and loops for ever.
        let problems: Vec<String> = (0..5000)
            .map(|i| {
                format!("file-{i}.txt: fingerprint 000000000000, but the request says 111111111111")
            })
            .collect();
        let args = revise_for(&problems, 1, "claude:1");
        assert!(
            args.body.chars().count() <= MAX_BODY,
            "{}",
            args.body.chars().count()
        );
        assert!(args
            .body
            .starts_with("REVISE round=1 blocking=5000 important=0"));
        assert!(
            args.body.contains("more"),
            "the truncation is said, not silent"
        );
        let long = vec!["x".repeat(MAX_BODY * 2)];
        assert!(revise_for(&long, 1, "claude:1").body.chars().count() <= MAX_BODY);
    }

    #[test]
    fn only_a_required_reviewer_in_the_current_unit_reviews() {
        let (_d, root) = fixture();
        let body = "Fingerprints:\n";
        // An adviser is never asked for a verdict.
        edit_session(
            &root,
            "authority",
            json!({"required_reviewers": ["claude"], "advisers": ["localpilot"]}),
        );
        assert!(matches!(
            one(mesh(&root).plan("localpilot", &review(body)).unwrap()),
            Step::Ack { ref why, .. } if why == "not a required reviewer"
        ));
        edit_session(
            &root,
            "authority",
            json!({"required_reviewers": ["localpilot"], "advisers": []}),
        );
        // A request from an earlier unit is stale.
        edit_session(&root, "unit_id", json!("2-def"));
        assert!(matches!(
            one(mesh(&root).plan("localpilot", &review(body)).unwrap()),
            Step::Ack { ref why, .. } if why == "from an earlier unit"
        ));
    }

    #[test]
    fn each_kind_gets_its_mechanical_step() {
        let (_d, root) = fixture();
        let m = mesh(&root);
        let step = |d: Delivery| one(m.plan("localpilot", &d).unwrap());
        assert!(matches!(
            step(mail(message("claude", 1, "NOTE", "fyi", false))),
            Step::Ack { ref why, .. } if why == "no reply owed"
        ));
        assert!(matches!(
            step(mail(message("claude", 2, "STOP", "halt now\nmore", false))),
            Step::Stop { ref reason, .. } if reason == "halt now"
        ));
        match step(mail(message("claude", 3, "HANDOFF_OFFER", "epoch=2", true))) {
            Step::Post { args, .. } => {
                assert_eq!(args.kind, "NOTE");
                assert_eq!(args.reply_to.as_deref(), Some("claude:3"));
            }
            other => panic!("{other:?}"),
        }
        match step(mail(message("claude", 4, "PLAN", "the plan", true))) {
            Step::Judge(req) => {
                assert_eq!(req.need, Need::Reply);
                assert!(!req.expect.reviewer);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            step(Delivery::Notice("PEER_HEALTH claude status=ready".into())),
            Step::Notice("PEER_HEALTH claude status=ready".into())
        );
    }

    #[test]
    fn a_message_already_answered_is_only_acknowledged() {
        // Bug it prevents: a crash between the post and the ack producing a
        // second verdict on redelivery.
        let (_d, root) = fixture();
        let mut reply = message("localpilot", 1, "ANSWER", "done", false);
        reply.insert("reply_to".into(), json!("claude:4"));
        crate::jsonl::append(
            &root
                .join(".pair-programming")
                .join("sessions")
                .join(SID)
                .join("journal")
                .join("localpilot.jsonl"),
            &reply,
        )
        .unwrap();
        assert!(mesh(&root).replied("localpilot", "claude:4").unwrap());
        assert!(matches!(
            one(mesh(&root).plan("localpilot", &mail(message("claude", 4, "PLAN", "p", true))).unwrap()),
            Step::Ack { ref why, .. } if why == "already answered"
        ));
    }

    #[test]
    fn a_schema_one_session_is_refused() {
        let (_d, root) = fixture();
        edit_session(&root, "schema", json!(1));
        let err = mesh(&root).plan(
            "localpilot",
            &mail(message("claude", 1, "NOTE", "x", false)),
        );
        assert!(err.is_err());
    }

    fn request(need: Need) -> Request {
        Request {
            need,
            msg_id: "claude:7".into(),
            kind: if need == Need::Review {
                "REVIEW_REQUEST".into()
            } else {
                "PLAN".into()
            },
            from: "claude".into(),
            body: String::new(),
            round: 2,
            files: vec!["a.txt".into()],
            expect: Expect {
                session_id: SID.into(),
                unit_id: Some(UNIT.into()),
                reviewer: need == Need::Review,
            },
        }
    }

    fn answer(kind: &str, decision: Option<&str>, findings: Vec<Finding>, body: &str) -> Answer {
        Answer {
            kind: kind.into(),
            decision: decision.map(str::to_owned),
            findings,
            body: body.into(),
        }
    }

    fn finding(severity: Severity) -> Finding {
        Finding {
            file: "a.txt".into(),
            line: Some(3),
            severity,
            text: "wrong".into(),
        }
    }

    #[test]
    fn the_engine_writes_the_verdict_header_from_the_findings() {
        let post = validate(
            &request(Need::Review),
            &answer(
                "VERDICT",
                Some("REVISE"),
                vec![
                    finding(Severity::Blocking),
                    finding(Severity::Important),
                    finding(Severity::Minor),
                ],
                "see above",
            ),
        )
        .unwrap();
        assert_eq!(post.kind, "VERDICT");
        assert_eq!(post.reply_to.as_deref(), Some("claude:7"));
        assert_eq!(
            post.body,
            "REVISE round=2 blocking=1 important=1\n- a.txt:3 [blocking] wrong\n- a.txt:3 [important] wrong\n- a.txt:3 [minor] wrong\nsee above"
        );
        let agree = validate(
            &request(Need::Review),
            &answer("VERDICT", Some("AGREE"), vec![], "fine"),
        )
        .unwrap();
        assert_eq!(agree.body, "AGREE round=2 blocking=0 important=0\nfine");
    }

    #[test]
    fn answers_the_protocol_does_not_allow_are_refused() {
        let review = request(Need::Review);
        let reply = request(Need::Reply);
        let refused = [
            (
                &review,
                answer(
                    "VERDICT",
                    Some("AGREE"),
                    vec![finding(Severity::Blocking)],
                    "x",
                ),
            ),
            (&review, answer("VERDICT", Some("REVISE"), vec![], "x")),
            (&review, answer("VERDICT", Some("LGTM"), vec![], "x")),
            (&review, answer("VERDICT", None, vec![], "x")),
            (&review, answer("ANSWER", None, vec![], "x")),
            (&review, answer("VERDICT", Some("AGREE"), vec![], "   ")),
            (
                &review,
                answer("VERDICT", Some("AGREE"), vec![], &"x".repeat(MAX_BODY)),
            ),
            (&reply, answer("VERDICT", Some("AGREE"), vec![], "x")),
            (&reply, answer("STOP", None, vec![], "x")),
            (&reply, answer("ANSWER", Some("AGREE"), vec![], "x")),
            (
                &reply,
                answer("ANSWER", None, vec![finding(Severity::Minor)], "x"),
            ),
        ];
        for (req, a) in refused {
            assert!(validate(req, &a).is_err(), "{a:?}");
        }
        let q = validate(&reply, &answer("QUESTION", None, vec![], "why?")).unwrap();
        assert!(q.expect_reply);
        let a = validate(&reply, &answer("DESIGN_AGREED", None, vec![], "ok")).unwrap();
        assert!(!a.expect_reply);
        assert_eq!(a.reply_to.as_deref(), Some("claude:7"));
    }

    #[test]
    fn the_answer_is_the_last_json_object_that_parses_as_one() {
        let text = "Thinking {not json}\n```json\n{\"kind\": \"ANSWER\", \"body\": \"first\"}\n```\nthen\n{\"kind\": \"ANSWER\", \"body\": \"second\"}\nbye";
        assert_eq!(parse_answer(text).unwrap().body, "second");
        let nested = "{\"kind\":\"VERDICT\",\"decision\":\"REVISE\",\"findings\":[{\"file\":\"a\",\"severity\":\"blocking\",\"text\":\"t\"}],\"body\":\"b\"}";
        assert_eq!(parse_answer(nested).unwrap().findings.len(), 1);
        assert!(parse_answer("no json here").is_err());
        let err = parse_answer("{\"kind\": \"ANSWER\", \"text\": \"wrong key\"}").unwrap_err();
        assert!(err.contains("not a valid answer"), "{err}");
    }

    #[test]
    fn an_escalation_names_the_message_and_never_carries_model_text() {
        let e = escalation(&request(Need::Review), "the body is empty");
        assert_eq!(e.kind, "ESCALATE");
        assert_eq!(e.reply_to.as_deref(), Some("claude:7"));
        assert!(
            e.body
                .contains("valid VERDICT for claude:7: the body is empty"),
            "{}",
            e.body
        );
    }

    #[test]
    fn a_guarded_post_lands_only_in_the_unit_it_was_planned_for() {
        // Bug it prevents: a verdict planned in one unit stamped into the
        // next because the session moved between the check and the post.
        let (_d, root) = fixture();
        let m = mesh(&root);
        let expect = Expect {
            session_id: SID.into(),
            unit_id: Some(UNIT.into()),
            reviewer: true,
        };
        let args = PostArgs {
            kind: "ANSWER".into(),
            body: "hello".into(),
            ..PostArgs::default()
        };
        let journal = root
            .join(".pair-programming")
            .join("sessions")
            .join(SID)
            .join("journal")
            .join("localpilot.jsonl");
        edit_session(&root, "unit_id", json!("2-def"));
        let err = m.post_guarded("localpilot", &args, &expect).unwrap_err();
        assert!(
            err.to_string().contains("STALE the work unit changed"),
            "{err}"
        );
        assert!(crate::jsonl::records(&journal).unwrap().is_empty());
        edit_session(&root, "unit_id", json!(UNIT));
        edit_session(&root, "owner", json!("localpilot"));
        let err = m.post_guarded("localpilot", &args, &expect).unwrap_err();
        assert!(
            err.to_string().contains("no longer a required reviewer"),
            "{err}"
        );
        edit_session(&root, "owner", json!("claude"));
        let posted = m.post_guarded("localpilot", &args, &expect).unwrap();
        assert_eq!(str_of(&posted, "unit_id"), Some(UNIT));
        assert_eq!(crate::jsonl::records(&journal).unwrap().len(), 1);
    }

    #[test]
    fn receive_delivers_like_peek_and_redelivers_until_acknowledged() {
        let (_d, root) = fixture();
        let m = mesh(&root);
        crate::jsonl::append(
            &root
                .join(".pair-programming")
                .join("sessions")
                .join(SID)
                .join("journal")
                .join("claude.jsonl"),
            &message("claude", 1, "NOTE", "hi", false),
        )
        .unwrap();
        let first = m.receive("localpilot", 900, None).unwrap().unwrap();
        let Delivery::Mail { messages, .. } = &first else {
            panic!("{first:?}")
        };
        assert_eq!(messages[0].count, 1);
        assert!(first.render().contains("hi"));
        let again = m.receive("localpilot", 900, None).unwrap().unwrap();
        let Delivery::Mail { messages, .. } = &again else {
            panic!("{again:?}")
        };
        assert_eq!(
            messages[0].count, 2,
            "unacknowledged mail is delivered again"
        );
        m.ack("localpilot", "claude:1").unwrap();
        assert_eq!(m.receive("localpilot", 900, None).unwrap(), None);
    }
}
