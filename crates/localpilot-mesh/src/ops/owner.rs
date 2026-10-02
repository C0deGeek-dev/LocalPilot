//! The owner half of the participant engine: what an owner owes next,
//! decided from the mailbox alone, and the review request built from the
//! tree rather than from a model.
//!
//! - [`Mesh::owner_state`] says whether this participant owns the unit and,
//!   if so, whether to implement (with any `REVISE` findings to answer), wait
//!   for verdicts on its open request, or close on its reviewers' agreement.
//!   It reads the journals afresh each time, so a crash between a model turn
//!   and its request just implements again, and a crash after the request
//!   just waits: a request is never posted twice, and unreviewed work never
//!   counts as done.
//! - [`Mesh::unit_changed_paths`] is the one definition of "changed in this
//!   unit" (everything committed since the unit's base plus everything
//!   uncommitted), shared by the owner's manifest and the reviewer's check.
//! - [`Mesh::review_request`] fingerprints every changed path itself; a path
//!   it cannot fingerprint is refused, never left out.

use serde_json::Value;

use super::engine::fingerprint_of;
use super::post::{Expect, PostArgs};
use super::read::num_at;
use super::unit::git;
use super::{authority, participants, seq_of, sid, str_of, Mesh, Obj, MAX_BODY};
use crate::error::MeshError;
use crate::jsonl;
use crate::layout::MAILBOX_DIR;

/// What the owner is asked to do in a round.
#[derive(Debug, Clone, PartialEq)]
pub struct OwnerTask {
    /// The session's task.
    pub task: String,
    /// Recent messages to this participant in the unit, as `ROLE KIND ID: body`.
    pub context: Vec<String>,
    /// The latest `REVISE` verdicts on this participant's request, if any.
    pub findings: Vec<String>,
    /// This round's number: this participant's requests in the unit plus one.
    pub round: i64,
    pub expect: Expect,
}

/// Where the owner stands.
#[derive(Debug, Clone, PartialEq)]
pub enum OwnerState {
    /// Not the owner of an active unit (or a handoff is pending).
    NotOwner,
    /// A request is open and not every required reviewer has answered it.
    Waiting(String),
    /// Work is owed: a first round, or an answer to `REVISE` findings.
    Implement(OwnerTask),
    /// Every required reviewer agreed with the latest request.
    Agreed(String),
    /// A required reviewer escalated or stopped the owner's work: the
    /// engine stops, and a person or another participant takes over.
    Escalated(String),
}

/// How much of the recent conversation an owner is shown.
const CONTEXT_MESSAGES: usize = 4;
const CONTEXT_CHARS: usize = 3_000;

/// Engine-owned progress notes are intermediate evidence, never review requests.
pub const OWNER_CHECKPOINT_PREFIX: &str = "Verified owner checkpoint (Done, Passed):";

fn is_agree(m: &Obj) -> bool {
    str_of(m, "body")
        .and_then(|b| b.lines().next())
        .is_some_and(|l| l.starts_with("AGREE "))
}

impl Mesh {
    /// Where `role` stands as an owner, from one read of the session and the
    /// journals.
    ///
    /// # Errors
    /// A refusal reading the session or a journal.
    pub fn owner_state(&self, role: &str) -> Result<OwnerState, MeshError> {
        let s = self.require(role, true)?;
        let handoff = s.get("handoff").is_some_and(|h| !h.is_null());
        if str_of(&s, "status") != Some("active") || str_of(&s, "owner") != Some(role) || handoff {
            return Ok(OwnerState::NotOwner);
        }
        let unit = str_of(&s, "unit_id").map(str::to_owned);
        let in_unit = |m: &Obj| str_of(m, "unit_id").map(str::to_owned) == unit;
        let mine: Vec<Obj> = jsonl::records(&self.mb.journal(sid(&s), role))?
            .into_iter()
            .filter(&in_unit)
            .collect();
        // Only this ownership counts: a request from before a hand-away and
        // back again was answered, or not, for an owner that no longer is.
        let since = mine
            .iter()
            .filter(|m| str_of(m, "kind") == Some("HANDOFF_ACCEPT"))
            .map(seq_of)
            .max()
            .unwrap_or(0);
        let requests: Vec<&Obj> = mine
            .iter()
            .filter(|m| str_of(m, "kind") == Some("REVIEW_REQUEST") && seq_of(m) > since)
            .collect();
        // A reviewer's decisions at or below its verdict floor no longer
        // stand (spec U-4, U-5).
        let floor = s
            .get("verdict_floor")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut peers: Vec<Obj> = Vec::new();
        for p in participants(&s).into_iter().filter(|p| p != role) {
            peers.extend(
                jsonl::records(&self.mb.journal(sid(&s), &p))?
                    .into_iter()
                    .filter(&in_unit),
            );
        }
        let expect = Expect {
            session_id: sid(&s).to_owned(),
            unit_id: unit.clone(),
            reviewer: false,
            owner: true,
        };
        let task = |findings: Vec<String>| {
            let mut context: Vec<String> = peers
                .iter()
                .filter(|m| {
                    !matches!(str_of(m, "kind"), Some("HELLO" | "VERDICT"))
                        && m.get("to")
                            .and_then(Value::as_array)
                            .is_none_or(|to| to.iter().any(|t| t.as_str() == Some(role)))
                })
                .rev()
                .take(CONTEXT_MESSAGES)
                .map(|m| {
                    let body: String = str_of(m, "body")
                        .unwrap_or_default()
                        .chars()
                        .take(CONTEXT_CHARS)
                        .collect();
                    format!(
                        "{} {} {}: {body}",
                        str_of(m, "role").unwrap_or_default(),
                        str_of(m, "kind").unwrap_or_default(),
                        str_of(m, "msg_id").unwrap_or_default()
                    )
                })
                .collect();
            context.reverse();
            if let Some(checkpoint) = mine.iter().rev().find(|m| {
                str_of(m, "kind") == Some("NOTE")
                    && seq_of(m) > since
                    && str_of(m, "body").is_some_and(|b| b.starts_with(OWNER_CHECKPOINT_PREFIX))
            }) {
                context.push(format!(
                    "Previous engine checkpoint (partial task; preserve acceptance criteria): {}",
                    str_of(checkpoint, "body")
                        .unwrap_or_default()
                        .chars()
                        .take(CONTEXT_CHARS)
                        .collect::<String>()
                ));
            }
            OwnerState::Implement(OwnerTask {
                task: str_of(&s, "task").unwrap_or_default().to_owned(),
                context,
                findings,
                round: i64::try_from(requests.len()).unwrap_or(i64::MAX) + 1,
                expect: expect.clone(),
            })
        };
        let (_, required, _) = authority(&s);
        let standing = |m: &&Obj, reviewer: &str| {
            str_of(m, "role") == Some(reviewer) && seq_of(m) > num_at(&floor, reviewer)
        };
        let latest_id = requests
            .last()
            .map(|m| str_of(m, "msg_id").unwrap_or_default().to_owned());
        // A standing STOP ends the owner's work whether or not a request is
        // open, so a restarted run cannot write past one it already read; an
        // ESCALATE counts when it answers the open request.
        for reviewer in &required {
            let stopped = peers.iter().find(|m| {
                standing(m, reviewer)
                    && (str_of(m, "kind") == Some("STOP")
                        || (str_of(m, "kind") == Some("ESCALATE")
                            && latest_id.is_some()
                            && str_of(m, "reply_to") == latest_id.as_deref()))
            });
            if let Some(m) = stopped {
                return Ok(OwnerState::Escalated(format!(
                    "{reviewer} {} {}: {}",
                    str_of(m, "kind").unwrap_or_default(),
                    str_of(m, "msg_id").unwrap_or_default(),
                    str_of(m, "body").unwrap_or_default()
                )));
            }
        }
        let Some(id) = latest_id else {
            return Ok(task(Vec::new()));
        };
        let mut revise = Vec::new();
        let mut agreed = 0usize;
        for reviewer in &required {
            let verdict = peers
                .iter()
                .filter(|m| {
                    standing(m, reviewer)
                        && str_of(m, "kind") == Some("VERDICT")
                        && str_of(m, "reply_to") == Some(id.as_str())
                })
                .next_back();
            match verdict {
                Some(v) if is_agree(v) => agreed += 1,
                Some(v) => revise.push(format!(
                    "{} VERDICT {}: {}",
                    reviewer,
                    str_of(v, "msg_id").unwrap_or_default(),
                    str_of(v, "body").unwrap_or_default()
                )),
                None => {}
            }
        }
        if !revise.is_empty() {
            return Ok(task(revise));
        }
        if agreed == required.len() && !required.is_empty() {
            return Ok(OwnerState::Agreed(id));
        }
        Ok(OwnerState::Waiting(id))
    }

    /// Whether this participant can work as an owner here: the owner path
    /// builds review requests from Git history, so a session without version
    /// control is refused before any model work.
    ///
    /// # Errors
    /// A refusal naming why.
    pub fn owner_supported(&self, role: &str) -> Result<(), MeshError> {
        let s = self.require(role, true)?;
        if self.no_vcs(&s)? {
            return Err(super::refused(
                "--own needs a Git anchor: a session without version control has no unit base to build a review request from",
            ));
        }
        Ok(())
    }

    /// Every path changed in the current unit, anchor-relative, the mailbox
    /// excluded: everything committed since the unit's base (the whole tree
    /// when the unit began before the first commit) plus everything
    /// uncommitted, both sides of a rename included.
    ///
    /// # Errors
    /// A refusal reading the session, or a tree Git cannot describe.
    pub fn unit_changed_paths(&self, role: &str) -> Result<Vec<String>, MeshError> {
        let s = self.require(role, true)?;
        self.changed_in_unit(&s)
    }

    pub(super) fn changed_in_unit(&self, s: &Obj) -> Result<Vec<String>, MeshError> {
        let root = self.repo();
        let unobserved =
            |what: &str| super::refused(format!("cannot observe the working tree: {what}"));
        let (rc, prefix) = git(root, &["rev-parse", "--show-prefix"])?;
        if rc != Some(0) {
            return Err(unobserved("git rev-parse failed"));
        }
        let prefix = prefix.trim().to_owned();
        let base = str_of(s, "base_head").unwrap_or("UNBORN");
        let mut paths: Vec<String> = Vec::new();
        let committed = if base == "UNBORN" {
            // No commit to diff from: everything committed since is the tree
            // at HEAD, if there is a HEAD at all.
            git(
                root,
                &["ls-tree", "-r", "--full-name", "--name-only", "-z", "HEAD"],
            )?
        } else {
            git(
                root,
                &["diff", "--name-only", "-z", &format!("{base}..HEAD")],
            )?
        };
        match committed {
            (Some(0), out) => {
                paths.extend(out.split('\0').filter(|p| !p.is_empty()).map(str::to_owned))
            }
            // An unborn HEAD has no commits since any base.
            _ if base == "UNBORN" => {}
            _ => return Err(unobserved("git diff against the unit base failed")),
        }
        let (rc, out) = git(
            root,
            &["status", "-z", "--porcelain=v1", "--untracked-files=all"],
        )?;
        if rc != Some(0) {
            return Err(unobserved("git status failed"));
        }
        let fields: Vec<&str> = out.split('\0').filter(|f| !f.is_empty()).collect();
        let mut i = 0;
        while i < fields.len() {
            let entry = fields[i];
            let (code, path) = (
                &entry[..entry.len().min(2)],
                entry.get(3..).unwrap_or_default(),
            );
            paths.push(path.to_owned());
            // A rename or copy, staged or not, names its source next.
            if code.contains('R') || code.contains('C') {
                if let Some(source) = fields.get(i + 1) {
                    paths.push((*source).to_owned());
                    i += 1;
                }
            }
            i += 1;
        }
        let mut out: Vec<String> = paths
            .into_iter()
            .filter_map(|p| p.strip_prefix(prefix.as_str()).map(str::to_owned))
            .filter(|p| {
                !p.is_empty() && p != MAILBOX_DIR && !p.starts_with(&format!("{MAILBOX_DIR}/"))
            })
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// The owner's review request for the unit: `summary`, then a
    /// `Fingerprints:` manifest of every changed path, fingerprinted here.
    /// Addressed to the required reviewers and awaiting their verdicts.
    ///
    /// # Errors
    /// The outer error for a refusal reading the session or the tree; the
    /// inner one when no request can honestly be made: nothing changed, a
    /// path that cannot be fingerprinted, or a request too long to post.
    pub fn review_request(
        &self,
        role: &str,
        summary: &str,
    ) -> Result<Result<PostArgs, String>, MeshError> {
        let s = self.require(role, true)?;
        let changed = self.changed_in_unit(&s)?;
        if changed.is_empty() {
            return Ok(Err(
                "nothing changed in the unit; there is nothing to review".into(),
            ));
        }
        let mut manifest = String::new();
        for path in &changed {
            match fingerprint_of(self.repo(), path) {
                Ok(Some(fp)) => manifest.push_str(&format!("{path}={fp}\n")),
                Ok(None) => manifest.push_str(&format!("{path}=deleted\n")),
                Err(why) => return Ok(Err(format!("{path} cannot be fingerprinted: {why}"))),
            }
        }
        let body = format!("{}\n\nFingerprints:\n{manifest}", summary.trim());
        if body.chars().count() > MAX_BODY {
            return Ok(Err(format!(
                "the request would exceed {MAX_BODY} characters ({} changed paths); split the work",
                changed.len()
            )));
        }
        let (_, required, _) = authority(&s);
        Ok(Ok(PostArgs {
            kind: "REVIEW_REQUEST".into(),
            body,
            expect_reply: true,
            to: Some(required.join(",")),
            ..PostArgs::default()
        }))
    }

    /// Build a durable intermediate note using the same fingerprint surface as
    /// review requests. The caller must establish native verified completion
    /// first and post with the owner's guarded expectation before continuing.
    pub fn owner_checkpoint(
        &self,
        role: &str,
        summary: &str,
    ) -> Result<Result<PostArgs, String>, MeshError> {
        Ok(self
            .review_request(role, &format!("{OWNER_CHECKPOINT_PREFIX}\n{summary}"))?
            .map(|mut args| {
                args.kind = "NOTE".into();
                args.expect_reply = false;
                args
            }))
    }

    /// Count durable checkpoints in this ownership, including before a restart.
    /// A handoff ends the allowance; a raw Git commit or a new process does not.
    pub fn owner_checkpoint_count(&self, role: &str) -> Result<usize, MeshError> {
        let s = self.require(role, true)?;
        let records = jsonl::records(&self.mb.journal(sid(&s), role))?;
        let in_unit = |m: &&Obj| str_of(m, "unit_id") == str_of(&s, "unit_id");
        let since = records
            .iter()
            .filter(in_unit)
            .filter(|m| str_of(m, "kind") == Some("HANDOFF_ACCEPT"))
            .map(seq_of)
            .max()
            .unwrap_or(0);
        Ok(records
            .iter()
            .filter(in_unit)
            .filter(|m| {
                seq_of(m) > since
                    && str_of(m, "kind") == Some("NOTE")
                    && str_of(m, "body")
                        .is_some_and(|body| body.starts_with(OWNER_CHECKPOINT_PREFIX))
            })
            .count())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    const SID: &str = "20260101T000000Z-abcdef12";
    const UNIT: &str = "1-abc";

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// A repository with a schema-2 session owned by localpilot and reviewed
    /// by claude. With `commit_first`, `a.txt` is committed; with
    /// `base_is_head`, the unit began at that commit, else before any.
    fn fixture(commit_first: bool, base_is_head: bool) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.email", "t@example.invalid"]);
        git(&root, &["config", "user.name", "t"]);
        git(&root, &["config", "core.autocrlf", "false"]);
        std::fs::create_dir_all(root.join(".git").join("info")).unwrap();
        std::fs::write(
            root.join(".git").join("info").join("exclude"),
            "/.pair-programming/\n",
        )
        .unwrap();
        let mut base = serde_json::Value::Null;
        if commit_first {
            std::fs::write(root.join("a.txt"), "alpha\n").unwrap();
            git(&root, &["add", "a.txt"]);
            git(&root, &["commit", "-qm", "base"]);
            if base_is_head {
                base = json!(git(&root, &["rev-parse", "HEAD"]));
            }
        }
        let mb = root.join(".pair-programming");
        let sd = mb.join("sessions").join(SID);
        std::fs::create_dir_all(sd.join("journal")).unwrap();
        let rec = json!({"session_id": SID, "task": "the task", "work_unit": "w", "unit_id": UNIT,
            "driver": "claude", "owner": "localpilot", "ownership_epoch": 2, "status": "active",
            "phase": "review", "waiting": null, "handoff": null, "vcs": "git", "base_head": base,
            "companions": [], "delivery": "ack", "protocol": "1.0", "schema": 2,
            "participants": ["claude", "localpilot"],
            "authority": {"required_reviewers": ["claude"], "advisers": []}, "pauses": {}});
        std::fs::write(sd.join("session.v2.json"), rec.to_string()).unwrap();
        std::fs::write(
            mb.join("active.json"),
            format!(
                "{}{SID}: see active.v2.json.\n",
                crate::layout::SENTINEL_PREFIX
            ),
        )
        .unwrap();
        std::fs::write(
            mb.join("active.v2.json"),
            json!({"session_id": SID, "status": "active", "schema": 2,
                   "participants": ["claude", "localpilot"]})
            .to_string(),
        )
        .unwrap();
        (dir, root)
    }

    fn journal(root: &Path, role: &str, records: &[serde_json::Value]) {
        let p = root
            .join(".pair-programming")
            .join("sessions")
            .join(SID)
            .join("journal")
            .join(format!("{role}.jsonl"));
        let text = records.iter().fold(String::new(), |mut text, r| {
            text.push_str(&r.to_string());
            text.push('\n');
            text
        });
        std::fs::write(p, text).unwrap();
    }

    fn msg(
        role: &str,
        seq: i64,
        kind: &str,
        body: &str,
        reply_to: Option<&str>,
    ) -> serde_json::Value {
        let to = if role == "claude" {
            "localpilot"
        } else {
            "claude"
        };
        json!({"seq": seq, "at": "2026-01-01T00:00:00Z", "role": role, "kind": kind, "work_unit": "w",
               "unit_id": UNIT, "expect_reply": kind == "REVIEW_REQUEST", "body": body,
               "msg_id": format!("{role}:{seq}"), "to": [to], "reply_to": reply_to,
               "broadcast": false, "thread_id": format!("{role}:{seq}"), "route_trace": [role],
               "ttl": null, "forward": false})
    }

    fn mesh(root: &Path) -> Mesh {
        Mesh::at(root, "flag")
    }

    #[test]
    fn a_committed_change_on_a_clean_tree_is_still_the_units_change() {
        // Bug it prevents: an owner that commits its work before asking for
        // review producing an empty review set.
        let (_d, root) = fixture(true, true);
        std::fs::write(root.join("b.txt"), "beta\n").unwrap();
        git(&root, &["add", "b.txt"]);
        git(&root, &["commit", "-qm", "work"]);
        assert_eq!(git(&root, &["status", "--porcelain"]), "");
        assert_eq!(
            mesh(&root).unit_changed_paths("localpilot").unwrap(),
            ["b.txt"]
        );
        let args = mesh(&root)
            .review_request("localpilot", "added b")
            .unwrap()
            .unwrap();
        assert!(
            args.body.starts_with("added b\n\nFingerprints:\nb.txt="),
            "{}",
            args.body
        );
        assert_eq!(args.to.as_deref(), Some("claude"));
        assert!(args.expect_reply);
    }

    #[test]
    fn a_unit_opened_before_the_first_commit_counts_the_whole_tree() {
        let (_d, root) = fixture(false, false);
        assert!(mesh(&root)
            .unit_changed_paths("localpilot")
            .unwrap()
            .is_empty());
        std::fs::write(root.join("c.txt"), "c\n").unwrap();
        git(&root, &["add", "c.txt"]);
        git(&root, &["commit", "-qm", "first"]);
        assert_eq!(
            mesh(&root).unit_changed_paths("localpilot").unwrap(),
            ["c.txt"]
        );
    }

    #[test]
    fn a_rename_names_both_sides_and_a_deletion_is_listed_as_deleted() {
        let (_d, root) = fixture(true, true);
        git(&root, &["mv", "a.txt", "renamed.txt"]);
        assert_eq!(
            mesh(&root).unit_changed_paths("localpilot").unwrap(),
            ["a.txt", "renamed.txt"]
        );
        let body = mesh(&root)
            .review_request("localpilot", "moved")
            .unwrap()
            .unwrap()
            .body;
        assert!(body.contains("a.txt=deleted\n"), "{body}");
        assert!(body.contains("renamed.txt="), "{body}");
    }

    #[test]
    fn nothing_changed_is_not_a_request() {
        let (_d, root) = fixture(true, true);
        let err = mesh(&root)
            .review_request("localpilot", "done")
            .unwrap()
            .unwrap_err();
        assert!(err.contains("nothing changed"), "{err}");
    }

    #[test]
    fn the_owner_state_follows_the_journals() {
        let (_d, root) = fixture(true, true);
        let m = mesh(&root);
        let OwnerState::Implement(t) = m.owner_state("localpilot").unwrap() else {
            panic!("expected work")
        };
        assert_eq!(
            (t.round, t.task.as_str(), t.findings.len()),
            (1, "the task", 0)
        );
        assert!(t.expect.owner);
        journal(
            &root,
            "localpilot",
            &[msg("localpilot", 1, "REVIEW_REQUEST", "r1", None)],
        );
        assert_eq!(
            m.owner_state("localpilot").unwrap(),
            OwnerState::Waiting("localpilot:1".into())
        );
        let revise = "REVISE round=1 blocking=1 important=0\n- fix it";
        journal(
            &root,
            "claude",
            &[msg("claude", 1, "VERDICT", revise, Some("localpilot:1"))],
        );
        let OwnerState::Implement(t) = m.owner_state("localpilot").unwrap() else {
            panic!("expected a second round")
        };
        assert_eq!(t.round, 2);
        assert!(t.findings[0].contains("fix it"), "{:?}", t.findings);
        journal(
            &root,
            "localpilot",
            &[
                msg("localpilot", 1, "REVIEW_REQUEST", "r1", None),
                msg("localpilot", 2, "REVIEW_REQUEST", "r2", None),
            ],
        );
        assert_eq!(
            m.owner_state("localpilot").unwrap(),
            OwnerState::Waiting("localpilot:2".into())
        );
        journal(
            &root,
            "claude",
            &[
                msg("claude", 1, "VERDICT", revise, Some("localpilot:1")),
                msg(
                    "claude",
                    2,
                    "VERDICT",
                    "AGREE round=2 blocking=0 important=0\nok",
                    Some("localpilot:2"),
                ),
            ],
        );
        assert_eq!(
            m.owner_state("localpilot").unwrap(),
            OwnerState::Agreed("localpilot:2".into())
        );
        assert_eq!(m.owner_state("claude").unwrap(), OwnerState::NotOwner);
    }

    #[test]
    fn checkpoint_progress_survives_a_fresh_owner_read_without_completing_the_unit() {
        let (_d, root) = fixture(true, true);
        std::fs::write(root.join("b.txt"), "partial\n").unwrap();
        let m = mesh(&root);
        let note = m
            .owner_checkpoint("localpilot", "source checked; required tests remain")
            .unwrap()
            .unwrap();
        assert_eq!(note.kind, "NOTE");
        assert!(!note.expect_reply);
        assert!(note.body.contains("b.txt="));
        journal(
            &root,
            "localpilot",
            &[msg("localpilot", 1, "NOTE", &note.body, None)],
        );
        let fresh = mesh(&root);
        assert_eq!(fresh.owner_checkpoint_count("localpilot").unwrap(), 1);
        let OwnerState::Implement(task) = fresh.owner_state("localpilot").unwrap() else {
            panic!("checkpoint completed work")
        };
        assert_eq!(task.task, "the task");
        assert_eq!(task.round, 1);
        assert!(task
            .context
            .iter()
            .any(|line| line.contains("required tests remain")));
    }

    #[test]
    fn a_reviewer_escalation_or_stop_ends_the_owners_work() {
        let (_d, root) = fixture(true, true);
        let m = mesh(&root);
        journal(
            &root,
            "localpilot",
            &[msg("localpilot", 1, "REVIEW_REQUEST", "r1", None)],
        );
        journal(
            &root,
            "claude",
            &[msg(
                "claude",
                1,
                "ESCALATE",
                "needs a person",
                Some("localpilot:1"),
            )],
        );
        let OwnerState::Escalated(why) = m.owner_state("localpilot").unwrap() else {
            panic!("an escalation must stop the owner")
        };
        assert!(why.contains("needs a person"), "{why}");
        journal(&root, "claude", &[msg("claude", 1, "STOP", "halt", None)]);
        assert!(matches!(
            m.owner_state("localpilot").unwrap(),
            OwnerState::Escalated(_)
        ));
    }

    #[test]
    fn a_stop_before_any_request_still_stops_a_restarted_owner() {
        let (_d, root) = fixture(true, true);
        journal(
            &root,
            "claude",
            &[msg("claude", 1, "STOP", "halt before you start", None)],
        );
        let OwnerState::Escalated(why) = mesh(&root).owner_state("localpilot").unwrap() else {
            panic!("a standing STOP must stop the owner before any request")
        };
        assert!(why.contains("halt before you start"), "{why}");
    }

    #[test]
    fn a_hand_away_and_back_needs_fresh_review() {
        // Bug it prevents: an agreement given to an earlier ownership closing
        // the unit, or a close the protocol refuses retried for ever.
        let (_d, root) = fixture(true, true);
        let m = mesh(&root);
        journal(
            &root,
            "localpilot",
            &[
                msg("localpilot", 1, "REVIEW_REQUEST", "r1", None),
                msg("localpilot", 2, "HANDOFF_OFFER", "away", None),
                msg("localpilot", 3, "HANDOFF_ACCEPT", "back", None),
            ],
        );
        journal(
            &root,
            "claude",
            &[msg(
                "claude",
                1,
                "VERDICT",
                "AGREE round=1 blocking=0 important=0\nok",
                Some("localpilot:1"),
            )],
        );
        let OwnerState::Implement(t) = m.owner_state("localpilot").unwrap() else {
            panic!("the old agreement must not stand")
        };
        assert_eq!(t.round, 1);
        // A verdict at or below the reviewer's floor no longer stands either.
        journal(
            &root,
            "localpilot",
            &[
                msg("localpilot", 1, "HANDOFF_ACCEPT", "back", None),
                msg("localpilot", 2, "REVIEW_REQUEST", "r2", None),
            ],
        );
        journal(
            &root,
            "claude",
            &[msg(
                "claude",
                1,
                "VERDICT",
                "AGREE round=1 blocking=0 important=0\nok",
                Some("localpilot:2"),
            )],
        );
        let rec = root
            .join(".pair-programming")
            .join("sessions")
            .join(SID)
            .join("session.v2.json");
        let mut s: Obj = serde_json::from_str(&std::fs::read_to_string(&rec).unwrap()).unwrap();
        s.insert("verdict_floor".into(), json!({"claude": 1}));
        std::fs::write(&rec, serde_json::Value::Object(s).to_string()).unwrap();
        assert_eq!(
            m.owner_state("localpilot").unwrap(),
            OwnerState::Waiting("localpilot:2".into())
        );
    }

    #[test]
    fn a_session_without_version_control_is_refused_as_an_owner() {
        let (_d, root) = fixture(true, true);
        let rec = root
            .join(".pair-programming")
            .join("sessions")
            .join(SID)
            .join("session.v2.json");
        let mut s: Obj = serde_json::from_str(&std::fs::read_to_string(&rec).unwrap()).unwrap();
        s.insert("vcs".into(), json!("none"));
        std::fs::write(&rec, serde_json::Value::Object(s).to_string()).unwrap();
        let err = mesh(&root).owner_supported("localpilot").unwrap_err();
        assert!(
            err.to_string().contains("--own needs a Git anchor"),
            "{err}"
        );
        let (_d2, git_root) = fixture(true, true);
        assert!(mesh(&git_root).owner_supported("localpilot").is_ok());
    }

    #[test]
    fn an_owner_post_after_losing_the_tree_is_refused() {
        let (_d, root) = fixture(true, true);
        std::fs::write(root.join("b.txt"), "beta\n").unwrap();
        let m = mesh(&root);
        let OwnerState::Implement(t) = m.owner_state("localpilot").unwrap() else {
            panic!("expected work")
        };
        let args = m.review_request("localpilot", "b").unwrap().unwrap();
        let rec = root
            .join(".pair-programming")
            .join("sessions")
            .join(SID)
            .join("session.v2.json");
        let mut s: Obj = serde_json::from_str(&std::fs::read_to_string(&rec).unwrap()).unwrap();
        s.insert("owner".into(), json!("claude"));
        std::fs::write(&rec, serde_json::Value::Object(s).to_string()).unwrap();
        let err = m.post_guarded("localpilot", &args, &t.expect).unwrap_err();
        assert!(
            err.to_string().contains("STALE no longer the owner"),
            "{err}"
        );
    }
}
