//! The participant operations (the specification's participant profile).
//!
//! Each operation reads and writes the shared mailbox exactly as the
//! specification says, under the same locks as every other implementation,
//! and returns what a command line would print ([`Out`]). Rendering lives
//! here rather than in a CLI because models read these lines, and the
//! conformance suite's observable layer checks them.

mod delivery;
mod join;
mod post;
mod read;
mod render;
mod unit;

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::error::MeshError;
use crate::fsio;
use crate::jsonl;
use crate::layout::{Mailbox, SENTINEL_PREFIX, SESSION_V1, SESSION_V2};
use crate::lock::Lock;
use crate::session;
use crate::timefmt::utc_now;

pub use delivery::{EndpointArgs, ENDPOINT_TOKEN_ENV, PUSH_OUTCOMES};
pub use post::PostArgs;
pub use read::WatchArgs;
pub use render::unit_label;
pub use unit::NextUnitArgs;

/// A JSON object, as records are held while an operation rewrites them.
pub type Obj = Map<String, Value>;

/// What an operation produced: an exit code and the text for each stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Out {
    pub code: u8,
    pub stdout: String,
    pub stderr: String,
}

impl Out {
    pub(crate) fn ok(stdout: impl Into<String>) -> Self {
        Self {
            code: 0,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }
    pub(crate) fn code(code: u8) -> Self {
        Self {
            code,
            ..Self::default()
        }
    }
}

/// Capabilities this implementation advertises at `join` (spec C-3).
pub const CAPS: &[&str] = &["ack"];
/// Forward edges a request may take from its author (spec A-5).
pub const FORWARD_TTL: i64 = 2;
/// Message kinds (spec M-3).
pub const KINDS: &[&str] = &[
    "HELLO",
    "PLAN",
    "CHALLENGE",
    "DESIGN_AGREED",
    "CHECKPOINT",
    "STOP",
    "STEER",
    "NOTE",
    "QUESTION",
    "ANSWER",
    "REVIEW_REQUEST",
    "VERDICT",
    "HANDOFF_OFFER",
    "HANDOFF_ACCEPT",
    "ESCALATE",
    "COMPLETE",
];
/// The only kind a participant may broadcast (spec A-4).
pub const BROADCAST_KINDS: &[&str] = &["ESCALATE"];
/// Health states (spec H-1).
pub const HEALTH: &[&str] = &[
    "not_joined",
    "ready",
    "working",
    "waiting",
    "rate_limited",
    "paused",
    "offline",
];
pub(crate) const DOWN: &[&str] = &["rate_limited", "paused", "offline"];
/// The longest body a post may carry (spec M-4).
pub const MAX_BODY: usize = 12_000;

/// One mailbox, operated as a participant.
#[derive(Debug, Clone)]
pub struct Mesh {
    mb: Mailbox,
    anchor: PathBuf,
    source: String,
}

impl Mesh {
    /// The mailbox of the working tree rooted at `anchor`; `source` says how
    /// the anchor was chosen (`flag`, `env` or `cwd`), for the `ANCHOR` line.
    #[must_use]
    pub fn at(anchor: &Path, source: &str) -> Self {
        Self {
            mb: Mailbox::at(anchor),
            anchor: anchor.to_path_buf(),
            source: source.to_owned(),
        }
    }

    /// The underlying mailbox paths.
    #[must_use]
    pub fn mailbox(&self) -> &Mailbox {
        &self.mb
    }

    fn anchor_line(&self) -> String {
        format!("ANCHOR={} source={}", self.anchor.display(), self.source)
    }

    // --- sessions -----------------------------------------------------------

    fn active(&self) -> Result<Option<Obj>, MeshError> {
        session::active_record(&self.mb)
    }

    fn require(&self, role: &str, allow_paused: bool) -> Result<Obj, MeshError> {
        let Some(s) = self.active()? else {
            return Err(refused("NO_ACTIVE_SESSION"));
        };
        let parts = participants(&s);
        if !parts.iter().any(|p| p == role) {
            return Err(refused(format!(
                "role not in active session (participants: {})",
                parts.join(", ")
            )));
        }
        match status(&s) {
            "completed" | "abandoned" => Err(refused("SESSION_CLOSED")),
            "parked" => Err(refused("SESSION_PARKED")),
            "paused" if !allow_paused => Err(refused("SESSION_PAUSED")),
            _ => Ok(s),
        }
    }

    /// Write the session record, then its pointer (spec S-7 ordering).
    fn save(&self, s: &Obj) -> Result<(), MeshError> {
        let sid = sid(s);
        if schema(s) == 1 {
            fsio::write_json(&self.mb.session_dir(sid).join(SESSION_V1), s)?;
            fsio::write_json(
                &self.mb.active_v1(),
                &json!({"session_id": sid, "driver": s.get("driver"), "status": s.get("status"), "updated_at": s.get("updated_at")}),
            )?;
            return remove_if_present(&self.mb.active_v2());
        }
        fsio::write_json(&self.mb.session_dir(sid).join(SESSION_V2), s)?;
        if matches!(status(s), "active" | "paused") {
            self.point(s)
        } else {
            self.unpoint(s)
        }
    }

    fn point(&self, s: &Obj) -> Result<(), MeshError> {
        let sid = sid(s);
        let sentinel = format!(
            "{SENTINEL_PREFIX}{sid}: this mailbox needs a pair-programming build that supports N-party sessions. See active.v2.json.\n"
        );
        localpilot_store::atomic_write(&self.mb.active_v1(), sentinel.as_bytes())?;
        fsio::write_json(
            &self.mb.active_v2(),
            &json!({"session_id": sid, "driver": s.get("driver"), "status": s.get("status"), "updated_at": utc_now(),
                    "participants": participants(s), "schema": 2}),
        )
    }

    fn unpoint(&self, s: &Obj) -> Result<(), MeshError> {
        let sid = sid(s);
        let p2 = self.mb.active_v2();
        if let Some(Value::Object(a)) = fsio::read_json::<Value>(&p2, "active.v2.json")? {
            if a.get("session_id").and_then(Value::as_str) == Some(sid) {
                remove_if_present(&p2)?;
            }
        }
        if let Some(raw) = fsio::read_bytes(&self.mb.active_v1())? {
            if raw.starts_with(format!("{SENTINEL_PREFIX}{sid}:").as_bytes()) {
                remove_if_present(&self.mb.active_v1())?;
            }
        }
        Ok(())
    }

    /// Record `role`'s health; the generation moves only on a change.
    fn set_health(
        &self,
        s: &Obj,
        role: &str,
        status: &str,
        reason: Option<&str>,
        resume: Option<&str>,
    ) -> Result<(), MeshError> {
        let p = self.mb.health(sid(s), role);
        let old: Obj = fsio::read_json(&p, "health")?.unwrap_or_default();
        let changed = old.get("status").and_then(Value::as_str) != Some(status)
            || old.get("reason").and_then(Value::as_str) != reason
            || old.get("resume_at").and_then(Value::as_str) != resume;
        let gen = old.get("generation").and_then(Value::as_i64).unwrap_or(0)
            + i64::from(changed || old.is_empty());
        fsio::write_json(
            &p,
            &json!({"role": role, "status": status, "generation": gen, "updated_at": utc_now(),
                    "reason": reason, "resume_at": resume}),
        )
    }

    fn health_of(&self, s: &Obj, role: &str) -> Result<Obj, MeshError> {
        Ok(fsio::read_json(&self.mb.health(sid(s), role), "health")?.unwrap_or_default())
    }

    // --- messages -----------------------------------------------------------

    /// The message `<role>:<seq>` in this session, or `None`.
    fn find_msg(&self, s: &Obj, msg_id: &str) -> Result<Option<Obj>, MeshError> {
        let Some((role, seq)) = parse_msg_id(msg_id) else {
            return Ok(None);
        };
        if !participants(s).iter().any(|p| p == role) {
            return Ok(None);
        }
        let p = self.mb.journal(sid(s), role);
        Ok(jsonl::records(&p)?.into_iter().find(|m| seq_of(m) == seq))
    }

    /// The message `msg_id`, only when `role` may see it.
    fn find_visible(&self, s: &Obj, msg_id: &str, role: &str) -> Result<Option<Obj>, MeshError> {
        Ok(self.find_msg(s, msg_id)?.filter(|m| visible(m, role)))
    }

    fn state_lock(&self) -> Result<Lock, MeshError> {
        Lock::acquire(&self.mb.state_lock())
    }
}

// --- record helpers ----------------------------------------------------------

pub(crate) fn refused(msg: impl Into<String>) -> MeshError {
    MeshError::Refused(msg.into())
}

fn remove_if_present(p: &Path) -> Result<(), MeshError> {
    match std::fs::remove_file(p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(MeshError::io(p, e)),
    }
}

pub(crate) fn str_of<'a>(o: &'a Obj, k: &str) -> Option<&'a str> {
    o.get(k).and_then(Value::as_str)
}

pub(crate) fn sid(s: &Obj) -> &str {
    str_of(s, "session_id").unwrap_or_default()
}

pub(crate) fn status(s: &Obj) -> &str {
    str_of(s, "status").unwrap_or_default()
}

pub(crate) fn schema(s: &Obj) -> i64 {
    s.get("schema").and_then(Value::as_i64).unwrap_or(1)
}

pub(crate) fn seq_of(m: &Obj) -> i64 {
    m.get("seq").and_then(Value::as_i64).unwrap_or(0)
}

pub(crate) fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// The session's identities in their fixed order; schema 1 derives them.
pub(crate) fn participants(s: &Obj) -> Vec<String> {
    let listed = strings(s.get("participants"));
    if !listed.is_empty() {
        return listed;
    }
    [str_of(s, "driver"), str_of(s, "navigator")]
        .into_iter()
        .flatten()
        .map(str::to_owned)
        .collect()
}

pub(crate) fn others(s: &Obj, role: &str) -> Vec<String> {
    participants(s).into_iter().filter(|p| p != role).collect()
}

/// The one other participant of a two-participant session.
pub(crate) fn the_peer(s: &Obj, role: &str) -> Result<String, MeshError> {
    let o = others(s, role);
    match o.as_slice() {
        [one] => Ok(one.clone()),
        _ => Err(refused(format!(
            "not supported for three-party sessions yet: this command assumes one peer (participants: {})",
            participants(s).join(", ")
        ))),
    }
}

pub(crate) fn delivery(s: &Obj) -> &str {
    str_of(s, "delivery")
        .filter(|d| !d.is_empty())
        .unwrap_or("print")
}

/// (owner, required reviewers, advisers) of the current unit (spec U-1).
pub(crate) fn authority(s: &Obj) -> (String, Vec<String>, Vec<String>) {
    let owner = str_of(s, "owner").unwrap_or_default().to_owned();
    let rec = s.get("authority").and_then(Value::as_object);
    let advisers = strings(rec.and_then(|r| r.get("advisers")));
    let required = match rec.and_then(|r| r.get("required_reviewers")) {
        Some(v) if !v.is_null() => strings(Some(v)),
        _ => participants(s)
            .into_iter()
            .filter(|p| *p != owner && !advisers.contains(p))
            .collect(),
    };
    (owner, required, advisers)
}

/// Every recorded pause, by participant (spec H-2).
pub(crate) fn pauses(s: &Obj) -> Obj {
    if schema(s) == 2 {
        if let Some(v) = s.get("pauses") {
            return v.as_object().cloned().unwrap_or_default();
        }
    }
    match s.get("pause").and_then(Value::as_object) {
        Some(p) => {
            let mut m = Obj::new();
            m.insert(
                str_of(p, "role").unwrap_or_default().to_owned(),
                Value::Object(p.clone()),
            );
            m
        }
        None => Obj::new(),
    }
}

fn migrate_pauses(s: &mut Obj) {
    if schema(s) == 2 && !s.contains_key("pauses") {
        let p = pauses(s);
        s.insert("pauses".into(), Value::Object(p));
        s.remove("pause");
    }
}

pub(crate) fn pause_blocks(s: &Obj, role: &str) -> bool {
    let (owner, required, _) = authority(s);
    role == owner || required.iter().any(|r| r == role)
}

/// A lenient integer: records written by other builds may hold a number as
/// a float or a string.
pub(crate) fn num(v: &Value) -> i64 {
    match v {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Value::String(s) => s.trim().parse().unwrap_or(0),
        Value::Bool(b) => i64::from(*b),
        _ => 0,
    }
}

pub(crate) fn set_pause(s: &mut Obj, role: &str, rec: Value) {
    migrate_pauses(s);
    if schema(s) == 2 {
        let mut ps = s
            .get("pauses")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        ps.insert(role.to_owned(), rec);
        s.insert("pauses".into(), Value::Object(ps));
    } else {
        s.insert("pause".into(), rec);
    }
    settle_status(s);
}

fn settle_status(s: &mut Obj) {
    if !matches!(status(s), "active" | "paused") {
        return;
    }
    let blocked = pauses(s).keys().any(|r| pause_blocks(s, r));
    s.insert(
        "status".into(),
        json!(if blocked { "paused" } else { "active" }),
    );
}

pub(crate) fn clear_pause(s: &mut Obj, role: &str) {
    migrate_pauses(s);
    if schema(s) == 2 {
        let mut ps = s
            .get("pauses")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        ps.remove(role);
        s.insert("pauses".into(), Value::Object(ps));
    } else if s
        .get("pause")
        .and_then(Value::as_object)
        .and_then(|p| str_of(p, "role"))
        == Some(role)
    {
        s.insert("pause".into(), Value::Null);
    }
    settle_status(s);
}

/// Mail a role may see and refer to: its own, addressed to it, or broadcast.
pub(crate) fn visible(m: &Obj, role: &str) -> bool {
    str_of(m, "role") == Some(role)
        || strings(m.get("to")).iter().any(|r| r == role)
        || m.get("broadcast").and_then(Value::as_bool).unwrap_or(false)
}

fn parse_msg_id(id: &str) -> Option<(&str, i64)> {
    let (role, seq) = id.split_once(':')?;
    if role.is_empty() || !role.bytes().all(|b| b.is_ascii_lowercase()) {
        return None;
    }
    if seq.starts_with('0') || !seq.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((role, seq.parse().ok()?))
}

/// Schema 2's waits: `{msg_id: {from, kind, since, pending, answered_by}}`.
/// A scalar wait from an older build is read as the equivalent entry.
pub(crate) fn waiting_map(s: &Obj) -> Obj {
    let Some(w) = s.get("waiting").and_then(Value::as_object) else {
        return Obj::new();
    };
    if w.contains_key("from_role") {
        let mid = format!(
            "{}:{}",
            str_of(w, "from_role").unwrap_or_default(),
            w.get("seq").and_then(Value::as_i64).unwrap_or(0)
        );
        let mut m = Obj::new();
        m.insert(
            mid,
            json!({"from": w.get("from_role"), "kind": w.get("kind"), "since": w.get("since"),
                   "pending": [w.get("for_role")], "answered_by": []}),
        );
        return m;
    }
    w.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msg_ids_are_role_colon_positive_seq() {
        assert_eq!(parse_msg_id("claude:12"), Some(("claude", 12)));
        for bad in [
            "claude:0",
            "claude:07",
            "Claude:1",
            ":1",
            "claude:",
            "claude:x",
            "claude",
        ] {
            assert_eq!(parse_msg_id(bad), None, "{bad}");
        }
    }

    #[test]
    fn authority_defaults_to_every_non_owner_but_advisers() {
        let s: Obj = serde_json::from_value(json!({
            "owner": "claude", "participants": ["claude", "codex", "localpilot"], "schema": 2,
            "authority": {"advisers": ["localpilot"]}
        }))
        .unwrap();
        let (o, req, adv) = authority(&s);
        assert_eq!(
            (o.as_str(), req, adv),
            (
                "claude",
                vec!["codex".to_owned()],
                vec!["localpilot".to_owned()]
            )
        );
    }

    #[test]
    fn a_blocking_pause_pauses_the_session_and_clearing_it_resumes() {
        let mut s: Obj = serde_json::from_value(json!({
            "status": "paused", "owner": "claude", "participants": ["claude", "codex", "localpilot"], "schema": 2,
            "authority": {"advisers": ["localpilot"]},
            "pauses": {"codex": {"role": "codex"}, "localpilot": {"role": "localpilot"}}
        }))
        .unwrap();
        clear_pause(&mut s, "codex");
        assert_eq!(status(&s), "active", "an adviser's pause is informational");
    }
}
