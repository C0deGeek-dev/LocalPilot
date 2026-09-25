//! Ownership and the unit of work: `guard-write`, handoff, `complete` and
//! `next-unit` (spec U-1..U-6, K-1..K-4).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use super::post::PostArgs;
use super::read::num_at;
use super::{
    authority, participants, refused, schema, seq_of, settle_status, sid, str_of, strings,
    the_peer, Mesh, Obj, Out,
};
use crate::error::MeshError;
use crate::jsonl;
use crate::timefmt::utc_now;

/// Kinds that take a position the owner must not close over.
const DECISION_KINDS: &[&str] = &["VERDICT", "STOP", "ESCALATE", "CHALLENGE"];
/// Decisions no thread scopes away.
const GLOBAL_DECISIONS: &[&str] = &["STOP", "ESCALATE"];

/// The state a handoff or a unit boundary is pinned to.
struct Snap {
    head: String,
    status: Vec<String>,
}

/// Run git in `repo`: its exit code and stdout, or an error when git could
/// not be run at all.
fn git(repo: &Path, args: &[&str]) -> Result<(Option<i32>, String), MeshError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| MeshError::Unsupported(format!("cannot run git to observe the tree ({e})")))?;
    let text = String::from_utf8_lossy(&out.stdout)
        .trim_end_matches('\n')
        .to_owned();
    Ok((out.status.code(), text))
}

fn unobserved(what: &str) -> MeshError {
    MeshError::Refused(format!(
        "cannot observe the working tree ({what}); refusing to pin ownership to a state nobody saw"
    ))
}

/// The current commit, or `UNBORN` only for a repository whose branch has no
/// commit yet. Any other failure is an error, never a guess.
fn resolve_head(repo: &Path) -> Result<String, MeshError> {
    let (rc, head) = git(repo, &["rev-parse", "--verify", "--quiet", "HEAD"])?;
    if rc == Some(0) && !head.trim().is_empty() {
        return Ok(head.trim().to_owned());
    }
    // Unborn: HEAD names a branch, and that branch does not exist yet.
    let (rc, branch) = git(repo, &["symbolic-ref", "-q", "HEAD"])?;
    if rc != Some(0) || branch.trim().is_empty() {
        return Err(unobserved("git cannot resolve HEAD"));
    }
    let (rc, _) = git(repo, &["show-ref", "--verify", "--quiet", branch.trim()])?;
    if rc == Some(1) {
        return Ok("UNBORN".to_owned());
    }
    Err(unobserved("git cannot resolve HEAD"))
}

fn snap(repo: &Path) -> Result<Snap, MeshError> {
    let head = resolve_head(repo)?;
    let (rc, status) = git(repo, &["status", "--porcelain=v1", "--untracked-files=all"])?;
    if rc != Some(0) {
        return Err(unobserved("git status failed"));
    }
    Ok(Snap {
        head,
        status: status.lines().map(str::to_owned).collect(),
    })
}

fn first_line(m: &Obj) -> &str {
    str_of(m, "body")
        .and_then(|b| b.lines().next())
        .unwrap_or_default()
}

fn is_agree(m: &Obj) -> bool {
    str_of(m, "kind") == Some("VERDICT") && first_line(m).starts_with("AGREE ")
}

fn thread_of(m: &Obj) -> Option<&str> {
    str_of(m, "thread_id").or_else(|| str_of(m, "msg_id"))
}

/// A unit's slug from its task.
fn slug(task: &str) -> String {
    let mut out = String::new();
    for c in task.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let s: String = out.trim_matches('-').chars().take(48).collect();
    if s.is_empty() {
        "pair-task".to_owned()
    } else {
        s
    }
}

/// Eight random hex digits for a unit id.
fn unit_suffix() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_owned()
}

/// The reviewer sets after `new_owner` takes the unit over (spec U-5).
fn handoff_authority(s: &Obj, new_owner: &str) -> Result<Value, MeshError> {
    let (old, required, advisers) = authority(s);
    let order = participants(s);
    let mut req: Vec<String> = required
        .iter()
        .filter(|r| *r != new_owner)
        .cloned()
        .collect();
    let mut adv: Vec<String> = advisers.into_iter().filter(|r| r != new_owner).collect();
    if required.iter().any(|r| r == new_owner) && req.is_empty() {
        req.push(old);
    } else {
        adv.push(old);
    }
    if req.is_empty() {
        return Err(refused(
            "this handoff would leave no required reviewer; refusing",
        ));
    }
    let keep = |set: &[String]| -> Vec<String> {
        order.iter().filter(|r| set.contains(r)).cloned().collect()
    };
    Ok(json!({"required_reviewers": keep(&req), "advisers": keep(&adv)}))
}

/// Validate an `--advisers` declaration for a new unit (spec U-2).
fn declare_authority(parts: &[String], owner: &str, advisers: &str) -> Result<Value, MeshError> {
    let named: Vec<String> = advisers
        .split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty() && *x != "none")
        .map(str::to_owned)
        .collect();
    let mut uniq = named.clone();
    uniq.sort();
    uniq.dedup();
    if uniq.len() != named.len() {
        return Err(refused("--advisers names a participant twice"));
    }
    for x in &named {
        if !parts.contains(x) {
            return Err(refused(format!(
                "adviser {x} is not a participant (participants: {})",
                parts.join(", ")
            )));
        }
        if x == owner {
            return Err(refused(format!(
                "adviser {x} is the owner; the owner cannot advise on its own unit"
            )));
        }
    }
    let req: Vec<&String> = parts
        .iter()
        .filter(|x| *x != owner && !named.contains(x))
        .collect();
    if req.is_empty() {
        return Err(refused("--advisers leaves no required reviewer; at least one non-owner participant must be required to agree"));
    }
    let adv: Vec<&String> = parts.iter().filter(|x| named.contains(x)).collect();
    Ok(json!({"required_reviewers": req, "advisers": adv}))
}

/// What `next-unit` was asked to open.
#[derive(Debug, Clone, Default)]
pub struct NextUnitArgs {
    pub task: String,
    pub work_unit: Option<String>,
    /// `None` keeps the current advisers; `"none"` clears them.
    pub advisers: Option<String>,
}

impl Mesh {
    fn repo(&self) -> &Path {
        &self.anchor
    }

    /// Companion trees and sessions without version control need content
    /// digests this participant does not compute yet; refuse rather than pin
    /// a boundary that leaves them out.
    fn refuse_unpinnable(&self, s: &Obj) -> Result<(), MeshError> {
        if s.get("companions")
            .and_then(Value::as_array)
            .is_some_and(|c| !c.is_empty())
        {
            return Err(MeshError::Unsupported(
                "this session declares companion repositories; LocalPilot's participant does not pin companions yet".into(),
            ));
        }
        self.refuse_no_vcs(s)
    }

    fn journal_rows(&self, s: &Obj, role: &str) -> Result<Vec<Obj>, MeshError> {
        jsonl::records(&self.mb.journal(sid(s), role))
    }

    /// The peer's most recent message that takes a position on the work.
    fn latest_decision(&self, s: &Obj, role: &str) -> Result<Obj, MeshError> {
        Ok(self
            .journal_rows(s, role)?
            .into_iter()
            .filter(|m| {
                DECISION_KINDS.contains(&str_of(m, "kind").unwrap_or_default())
                    || m.get("expect_reply")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
            })
            .last()
            .unwrap_or_default())
    }

    /// Required reviewer `r`'s standing decision on the current unit (U-3b):
    /// `Ok(())` for a standing AGREE, else why not.
    fn reviewer_standing(&self, s: &Obj, r: &str) -> Result<Result<(), String>, MeshError> {
        let o = str_of(s, "owner").unwrap_or_default();
        let unit = s.get("unit_id");
        let floor = s
            .get("verdict_floor")
            .and_then(Value::as_object)
            .map_or(0, |f| num_at(f, r));
        let thread = self
            .journal_rows(s, o)?
            .into_iter()
            .filter(|m| {
                str_of(m, "kind") == Some("REVIEW_REQUEST")
                    && m.get("unit_id") == unit
                    && strings(m.get("to")).iter().any(|x| x == r)
            })
            .last()
            .and_then(|m| thread_of(&m).map(str::to_owned));
        let mut best: Option<Obj> = None;
        for m in self.journal_rows(s, r)? {
            if m.get("unit_id") != unit || seq_of(&m) <= floor || str_of(&m, "owner") == Some(r) {
                continue;
            }
            let k = str_of(&m, "kind").unwrap_or_default();
            let scoped = thread.is_some()
                && thread_of(&m) == thread.as_deref()
                && strings(m.get("to")).iter().any(|x| x == o)
                && (matches!(k, "VERDICT" | "CHALLENGE")
                    || m.get("expect_reply")
                        .and_then(Value::as_bool)
                        .unwrap_or(false));
            if (scoped || GLOBAL_DECISIONS.contains(&k))
                && best.as_ref().is_none_or(|b| seq_of(&m) > seq_of(b))
            {
                best = Some(m);
            }
        }
        let Some(best) = best else {
            return Ok(Err(if thread.is_none() {
                format!("{r}: no review requested from it in this unit")
            } else {
                format!("{r}: no verdict since the unit boundary or resume")
            }));
        };
        if !is_agree(&best) {
            return Ok(Err(format!(
                "{r}: {} #{} stands",
                str_of(&best, "kind").unwrap_or("None"),
                seq_of(&best)
            )));
        }
        Ok(Ok(()))
    }

    /// Refuse unless the unit's reviewers agree (U-3a for one, U-3b for more).
    fn peer_agreed(&self, s: &Obj, role: &str) -> Result<(), MeshError> {
        if participants(s).iter().filter(|x| *x != role).count() > 1 {
            let (_, required, _) = authority(s);
            let mut blocks = Vec::new();
            for r in &required {
                if let Err(why) = self.reviewer_standing(s, r)? {
                    blocks.push(why);
                }
            }
            if !blocks.is_empty() {
                return Err(refused(format!(
                    "not every required reviewer agrees: {}",
                    blocks.join("; ")
                )));
            }
            return Ok(());
        }
        let peer = the_peer(s, role)?;
        let d = self.latest_decision(s, &peer)?;
        if !is_agree(&d) {
            return Err(refused("peer's latest decision is not an AGREE verdict"));
        }
        if d.get("unit_id") != s.get("unit_id") {
            let show =
                |o: &Obj, k: &str| o.get(k).map_or_else(|| "None".to_owned(), Value::to_string);
            return Err(refused(format!(
                "peer's AGREE was given on work unit {} ({}), not the current {} ({}); a fresh verdict is required",
                show(&d, "work_unit"),
                show(&d, "unit_id"),
                show(s, "work_unit"),
                show(s, "unit_id")
            )));
        }
        let floor = s
            .get("verdict_floor")
            .and_then(Value::as_object)
            .map_or(0, |f| num_at(f, &peer));
        if seq_of(&d) <= floor {
            return Err(refused("peer's AGREE predates the last unit boundary or resume; the review was invalidated and a new verdict is required"));
        }
        Ok(())
    }

    /// `guard-write`: may `role` write now, and, with `path`, there?
    /// Every denial is exit 4 with a `WRITE_DENIED` line.
    ///
    /// # Errors
    /// A refusal from [`Mesh::require`]; a path in a companion tree.
    pub fn guard_write(&self, role: &str, path: Option<&Path>) -> Result<Out, MeshError> {
        let s = self.require(role, true)?;
        let handoff = s.get("handoff").is_some_and(|h| !h.is_null());
        if str_of(&s, "status") != Some("active") || str_of(&s, "owner") != Some(role) || handoff {
            return Ok(Out {
                code: 4,
                stdout: String::new(),
                stderr: format!(
                    "WRITE_DENIED status={} owner={} handoff={}\n",
                    str_of(&s, "status").unwrap_or("None"),
                    str_of(&s, "owner").unwrap_or("None"),
                    if handoff { "yes" } else { "no" }
                ),
            });
        }
        let Some(p) = path else {
            return Ok(Out::code(0));
        };
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|e| MeshError::io(p, e))?
                .join(p)
        };
        let q = resolve_lexically(&abs);
        let repo = resolve_lexically(self.repo());
        if q.starts_with(&repo) {
            return Ok(Out::code(0));
        }
        if s.get("companions")
            .and_then(Value::as_array)
            .is_some_and(|c| !c.is_empty())
        {
            return Err(MeshError::Unsupported(
                "this session declares companion repositories; LocalPilot's participant does not check companion write scopes yet".into(),
            ));
        }
        Ok(Out {
            code: 4,
            stdout: String::new(),
            stderr: format!(
                "WRITE_DENIED reason=outside-session-scope path={}\n  the session owns {}; a sibling repository is evidence, not workspace,\n  unless it is declared in .pair-companion.json and the session was started after that\n",
                q.display(),
                repo.display()
            ),
        })
    }

    /// `handoff-offer`: the owner offers the unit to `to`, pinned to the
    /// current tree.
    ///
    /// # Errors
    /// Not the owner; an ambiguous or invalid `to`; a handoff that would leave
    /// no required reviewer.
    pub fn handoff_offer(&self, role: &str, to: Option<&str>) -> Result<Out, MeshError> {
        let (epoch, to, head, dirty, multi, sid_) = {
            let _g = self.state_lock()?;
            let mut s = self.require(role, false)?;
            if str_of(&s, "owner") != Some(role) {
                return Err(refused("only owner may offer handoff"));
            }
            let others: Vec<String> = participants(&s).into_iter().filter(|r| r != role).collect();
            let to = match to {
                None => match others.as_slice() {
                    [one] => one.clone(),
                    _ => {
                        return Err(refused(format!(
                            "handoff-offer needs --to <participant> with {} participants",
                            participants(&s).len()
                        )))
                    }
                },
                Some(t) if others.iter().any(|o| o == t) => t.to_owned(),
                Some(t) => {
                    return Err(refused(format!(
                        "--to {t} is not another participant (participants: {})",
                        participants(&s).join(", ")
                    )))
                }
            };
            handoff_authority(&s, &to)?;
            self.refuse_unpinnable(&s)?;
            let q = snap(self.repo())?;
            let epoch = num_at(&s, "ownership_epoch") + 1;
            s.insert(
                "handoff".into(),
                json!({"epoch": epoch, "from": role, "to": to, "head": q.head, "status": q.status,
                       "companions": [], "offered_at": utc_now()}),
            );
            s.insert("updated_at".into(), json!(utc_now()));
            self.save(&s)?;
            (
                epoch,
                to,
                q.head,
                q.status.len(),
                others.len() > 1,
                sid(&s).to_owned(),
            )
        };
        let offer = PostArgs {
            kind: "HANDOFF_OFFER".into(),
            body: format!("epoch={epoch} to={to} head={head} dirty={dirty}"),
            expect_reply: true,
            to: multi.then(|| to.clone()),
            ..PostArgs::default()
        };
        self.post_message(role, &offer, Some(&sid_), true)?;
        Ok(Out::ok(format!("HANDOFF_OFFERED epoch={epoch} to={to}\n")))
    }

    /// `handoff-accept`: take the unit over, if the tree is as it was offered.
    ///
    /// # Errors
    /// No offer to this role; a tree that moved since the offer.
    pub fn handoff_accept(&self, role: &str) -> Result<Out, MeshError> {
        let (epoch, head, multi, sid_) = {
            let _g = self.state_lock()?;
            let mut s = self.require(role, false)?;
            let o = s
                .get("handoff")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            if o.is_empty() || str_of(&o, "to") != Some(role) {
                return Err(refused("no handoff offered to this role"));
            }
            self.refuse_unpinnable(&s)?;
            let q = snap(self.repo())?;
            if str_of(&o, "head") != Some(q.head.as_str()) || strings(o.get("status")) != q.status {
                return Err(refused(
                    "working tree changed since handoff offer; user must resolve ownership",
                ));
            }
            let old = str_of(&s, "owner").unwrap_or_default().to_owned();
            if schema(&s) == 2 {
                let auth = handoff_authority(&s, role)?;
                if strings(auth.get("required_reviewers")).contains(&old) {
                    let mut fl = s
                        .get("verdict_floor")
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default();
                    let seq = self
                        .read_obj(&self.mb.latest(sid(&s), &old))?
                        .map_or(0, |l| seq_of(&l));
                    fl.insert(old.clone(), json!(seq));
                    s.insert("verdict_floor".into(), Value::Object(fl));
                }
                s.insert("authority".into(), auth);
            }
            let epoch = o.get("epoch").cloned().unwrap_or(Value::Null);
            s.insert("owner".into(), json!(role));
            s.insert("ownership_epoch".into(), epoch.clone());
            s.insert("handoff".into(), Value::Null);
            s.insert("waiting".into(), Value::Null);
            settle_status(&mut s);
            s.insert("updated_at".into(), json!(utc_now()));
            self.save(&s)?;
            let head = str_of(&o, "head").unwrap_or_default().to_owned();
            (epoch, head, participants(&s).len() > 2, sid(&s).to_owned())
        };
        let accept = PostArgs {
            kind: "HANDOFF_ACCEPT".into(),
            body: format!("epoch={epoch} head={head}"),
            broadcast: multi,
            ..PostArgs::default()
        };
        self.post_message(role, &accept, Some(&sid_), true)?;
        Ok(Out::ok(format!(
            "HANDOFF_ACCEPTED epoch={epoch} owner={role}\n"
        )))
    }

    /// `complete`: the owner closes the session on its reviewers' agreement.
    ///
    /// # Errors
    /// Not the owner; no standing agreement.
    pub fn complete(&self, role: &str) -> Result<Out, MeshError> {
        let _g = self.state_lock()?;
        let mut s = self.require(role, false)?;
        if str_of(&s, "owner") != Some(role) {
            return Err(refused("only current owner may close"));
        }
        self.peer_agreed(&s, role)?;
        s.insert("status".into(), json!("completed"));
        s.insert("phase".into(), json!("complete"));
        s.insert("waiting".into(), Value::Null);
        s.insert("updated_at".into(), json!(utc_now()));
        self.save(&s)?;
        Ok(Out::ok(format!("COMPLETED session={}\n", sid(&s))))
    }

    /// `next-unit`: close the current unit on its reviewers' agreement and
    /// open the next one, in one write.
    ///
    /// # Errors
    /// No task; not the owner; a pending handoff; an invalid `--advisers`;
    /// no standing agreement.
    pub fn next_unit(&self, role: &str, a: &NextUnitArgs) -> Result<Out, MeshError> {
        let task = a.task.trim();
        if task.is_empty() {
            return Err(refused("the next work unit needs a task"));
        }
        let _g = self.state_lock()?;
        let mut s = self.require(role, false)?;
        if str_of(&s, "owner") != Some(role) {
            return Err(refused(
                "only the current owner may open the next work unit",
            ));
        }
        if s.get("handoff").is_some_and(|h| !h.is_null()) {
            return Err(refused(
                "a handoff is pending; settle ownership before opening the next work unit",
            ));
        }
        let (owner, required, advisers) = authority(&s);
        let adv = a.advisers.clone().unwrap_or_else(|| advisers.join(","));
        let auth = declare_authority(&participants(&s), &owner, &adv)?;
        self.peer_agreed(&s, role)?;
        self.refuse_unpinnable(&s)?;
        // Observe the tree before anything changes: a failed probe leaves the
        // unit open exactly as it was.
        let closed_head = resolve_head(self.repo())?;
        let base = snap(self.repo())?;
        let now = utc_now();
        let mut done: Vec<Value> = s
            .get("units")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let get = |k: &str| s.get(k).cloned().unwrap_or(Value::Null);
        let mut closed = json!({
            "work_unit": get("work_unit"), "unit_id": get("unit_id"), "task": get("task"),
            "opened_at": s.get("unit_opened_at").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| get("created_at")),
            "closed_at": now, "base_head": get("base_head"), "base_status": get("base_status"),
            "companions": get("companions"), "verdict_floor": get("verdict_floor"),
            "closed_head": closed_head,
        });
        if schema(&s) == 2 {
            closed["owner"] = json!(owner);
            closed["authority"] = json!({"required_reviewers": required, "advisers": advisers});
        }
        let closed_work = get("work_unit");
        done.push(closed);
        let n = done.len();
        s.insert("units".into(), json!(done));
        s.insert("task".into(), json!(task));
        let work = a
            .work_unit
            .clone()
            .filter(|w| !w.is_empty())
            .unwrap_or_else(|| slug(task));
        s.insert("work_unit".into(), json!(work));
        s.insert("unit_opened_at".into(), json!(now));
        s.insert(
            "unit_id".into(),
            json!(format!("{}-{}", n + 1, unit_suffix())),
        );
        let q = base;
        s.insert("base_head".into(), json!(q.head));
        s.insert("base_status".into(), json!(q.status));
        s.insert("companions".into(), json!([]));
        let mut floor = Obj::new();
        for r in participants(&s) {
            let seq = self
                .read_obj(&self.mb.latest(sid(&s), &r))?
                .map_or(0, |l| seq_of(&l));
            floor.insert(r, json!(seq));
        }
        s.insert("verdict_floor".into(), Value::Object(floor));
        if schema(&s) == 2 {
            s.insert("authority".into(), auth);
        }
        s.insert("phase".into(), json!("huddle"));
        s.insert("waiting".into(), Value::Null);
        settle_status(&mut s);
        s.insert("updated_at".into(), json!(utc_now()));
        self.save(&s)?;
        Ok(Out::ok(format!(
            "NEXT_UNIT session={} work={work} closed={} units={}\nREVIEW_INVALIDATED phase=huddle; a new peer AGREE is required before the next boundary or complete\n",
            sid(&s),
            closed_work.as_str().unwrap_or("None"),
            n + 1
        )))
    }
}

/// `path` made absolute and free of `.` and `..`, following no links, the
/// way a path is compared against the session's tree.
fn resolve_lexically(p: &Path) -> PathBuf {
    if let Ok(c) = dunce_canonical(p) {
        return c;
    }
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// `canonicalize` without Windows' verbatim `\\?\` prefix.
fn dunce_canonical(p: &Path) -> std::io::Result<PathBuf> {
    let c = std::fs::canonicalize(p)?;
    let s = c.to_string_lossy();
    Ok(match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => c,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: &str = "20260101T000000Z-abcdef12";

    /// A schema-1 session in `dir`, owned by claude, with codex's AGREE on
    /// the current unit and, optionally, a handoff offered to codex.
    fn session(dir: &Path, handoff: bool) -> PathBuf {
        let mb = dir.join(".pair-programming");
        let sd = mb.join("sessions").join(SID);
        std::fs::create_dir_all(sd.join("journal")).unwrap();
        let offer = if handoff {
            json!({"epoch": 1, "from": "claude", "to": "codex", "head": "UNBORN", "status": [], "companions": []})
        } else {
            Value::Null
        };
        let rec = json!({"session_id": SID, "driver": "claude", "navigator": "codex", "owner": "claude",
                         "status": "active", "phase": "review", "ownership_epoch": 0, "work_unit": "w",
                         "unit_id": "1-abc", "task": "t", "handoff": offer, "protocol": "1.0"});
        std::fs::write(sd.join("session.json"), rec.to_string()).unwrap();
        std::fs::write(
            mb.join("active.json"),
            json!({"session_id": SID, "driver": "claude", "status": "active", "updated_at": "2026-01-01T00:00:00Z"}).to_string(),
        )
        .unwrap();
        let agree = json!({"seq": 1, "at": "2026-01-01T00:00:00Z", "role": "codex", "kind": "VERDICT",
                           "work_unit": "w", "unit_id": "1-abc", "expect_reply": false, "body": "AGREE round=1"});
        std::fs::write(sd.join("journal").join("codex.jsonl"), format!("{agree}\n")).unwrap();
        sd.join("session.json")
    }

    fn unseen(r: Result<Out, MeshError>) -> bool {
        matches!(r, Err(MeshError::Refused(ref m)) if m.starts_with("cannot observe the working tree"))
    }

    #[test]
    fn a_tree_git_cannot_observe_pins_no_boundary() {
        // Not a Git working tree: every probe fails, and nothing may be read
        // as a clean, unborn tree.
        let dir = tempfile::tempdir().unwrap();
        assert!(snap(dir.path()).is_err());
        let rec = session(dir.path(), false);
        let before = std::fs::read(&rec).unwrap();
        let mesh = Mesh::at(dir.path(), "flag");
        assert!(unseen(mesh.handoff_offer("claude", None)));
        let a = NextUnitArgs {
            task: "next".into(),
            ..NextUnitArgs::default()
        };
        assert!(unseen(mesh.next_unit("claude", &a)));
        assert_eq!(
            std::fs::read(&rec).unwrap(),
            before,
            "a refused boundary changes nothing"
        );

        let dir = tempfile::tempdir().unwrap();
        let rec = session(dir.path(), true);
        let before = std::fs::read(&rec).unwrap();
        assert!(unseen(Mesh::at(dir.path(), "flag").handoff_accept("codex")));
        assert_eq!(std::fs::read(&rec).unwrap(), before);
    }

    #[test]
    fn an_unborn_repository_is_the_only_unborn_head() {
        let dir = tempfile::tempdir().unwrap();
        let ok = Command::new("git")
            .args(["init", "-q"])
            .arg(dir.path())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            return; // no git here; the failure path above still ran
        }
        let q = snap(dir.path()).unwrap();
        assert_eq!(q.head, "UNBORN");
        assert!(q.status.is_empty());
    }

    #[test]
    fn slugs_follow_the_task() {
        assert_eq!(slug("Fix the Parser!"), "fix-the-parser");
        assert_eq!(slug("!!!"), "pair-task");
        assert_eq!(slug(&"a".repeat(60)).len(), 48);
    }

    #[test]
    fn a_handoff_to_the_only_reviewer_makes_the_old_owner_required() {
        let s: Obj = serde_json::from_value(json!({
            "owner": "claude", "participants": ["claude", "codex"], "schema": 2
        }))
        .unwrap();
        let a = handoff_authority(&s, "codex").unwrap();
        assert_eq!(a, json!({"required_reviewers": ["claude"], "advisers": []}));
    }

    #[test]
    fn advisers_cannot_leave_nobody_required() {
        let parts = vec!["claude".to_owned(), "codex".to_owned()];
        assert!(declare_authority(&parts, "claude", "codex").is_err());
        assert_eq!(
            declare_authority(&parts, "claude", "none").unwrap(),
            json!({"required_reviewers": ["codex"], "advisers": []})
        );
    }
}
