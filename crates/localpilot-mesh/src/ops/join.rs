//! Joining, health, and the read-only views: `status` and `transcript`.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::post::PostArgs;
use super::render::{absent_line, authority_line, companion_line, pause_line, unit_label};
use super::{
    authority, clear_pause, delivery, participants, pauses, refused, schema, seq_of, set_pause,
    sid, status as status_of, str_of, strings, waiting_map, Mesh, Obj, Out, CAPS, HEALTH,
};
use crate::error::MeshError;
use crate::jsonl;
use crate::layout::{SESSION_V1, SESSION_V2};
use crate::session::check_protocol;
use crate::timefmt::{parse_utc, utc_now};

const VCS_FILE: &str = "vcs.json";

impl Mesh {
    /// Refuse a session anchored without version control: its review
    /// boundary is a content digest this implementation does not compute yet.
    pub(crate) fn refuse_no_vcs(&self, s: &Obj) -> Result<(), MeshError> {
        let stored: Option<Obj> = self.read_obj(&self.mb.base().join(VCS_FILE))?;
        let none = str_of(s, "vcs") == Some("none")
            || stored.as_ref().and_then(|m| str_of(m, "vcs")) == Some("none");
        if none {
            return Err(MeshError::Unsupported(
                "this session has no version control; LocalPilot's participant does not support content-digest sessions yet".into(),
            ));
        }
        Ok(())
    }

    /// `join`: wait for a session that names `role`, record readiness,
    /// advertise capabilities, and announce a real transition with a HELLO.
    ///
    /// # Errors
    /// A parked session, or a role the session does not name.
    pub fn join(&self, role: &str, timeout: u64, poll: Duration) -> Result<Out, MeshError> {
        let end = (timeout > 0).then(|| Instant::now() + Duration::from_secs(timeout));
        loop {
            if let Some(s) = self.active()? {
                if status_of(&s) == "parked" {
                    return Err(refused(format!(
                        "SESSION_PARKED session={} work={}; use `resume` to bring it back",
                        sid(&s),
                        str_of(&s, "work_unit").unwrap_or_default()
                    )));
                }
                if !matches!(status_of(&s), "completed" | "abandoned") {
                    if let Some(out) = self.join_once(role)? {
                        return Ok(out);
                    }
                    continue;
                }
            }
            if end.is_some_and(|e| Instant::now() >= e) {
                return Ok(Out::code(1));
            }
            std::thread::sleep(poll);
        }
    }

    /// One join attempt; `None` when the session changed under it.
    fn join_once(&self, role: &str) -> Result<Option<Out>, MeshError> {
        let (s, was) = {
            let _g = self.state_lock()?;
            let Some(mut s) = self.active()? else {
                return Ok(None);
            };
            if matches!(status_of(&s), "completed" | "abandoned" | "parked") {
                return Ok(None);
            }
            // Every refusal comes before any write.
            if !participants(&s).iter().any(|p| p == role) {
                return Err(refused(format!(
                    "role not in active session (participants: {})",
                    participants(&s).join(", ")
                )));
            }
            self.refuse_no_vcs(&s)?;
            let was = str_of(&self.health_of(&s, role)?, "status").map(str::to_owned);
            if pauses(&s).contains_key(role) {
                clear_pause(&mut s, role);
                s.insert("updated_at".into(), json!(utc_now()));
                self.save(&s)?;
            }
            let mut caps = s
                .get("caps")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let mut changed = strings(caps.get(role)) != CAPS;
            caps.insert(role.to_owned(), json!(CAPS));
            let all_ack = participants(&s)
                .iter()
                .all(|r| strings(caps.get(r)).iter().any(|c| c == "ack"));
            s.insert("caps".into(), Value::Object(caps));
            if delivery(&s) == "print" && all_ack {
                s.insert("delivery".into(), json!("ack"));
                changed = true;
            }
            if changed {
                s.insert("updated_at".into(), json!(utc_now()));
                self.save(&s)?;
            }
            self.set_health(&s, role, "ready", None, None)?;
            (s, was)
        };
        if str_of(&s, "driver") != Some(role) && was.as_deref() != Some("ready") {
            let hello = PostArgs {
                kind: "HELLO".into(),
                body: "joined; navigator ready".into(),
                broadcast: schema(&s) == 2,
                ..PostArgs::default()
            };
            self.post_message(role, &hello, Some(sid(&s)), true)?;
        }
        // The lock is released: re-confirm before announcing a task.
        match self.active()? {
            Some(cur) if sid(&cur) == sid(&s) => {}
            _ => return Ok(None),
        }
        let peers: Vec<String> = participants(&s).into_iter().filter(|p| p != role).collect();
        let rl = if str_of(&s, "driver") == Some(role) {
            "driver"
        } else {
            "navigator"
        };
        let work = str_of(&s, "work_unit").unwrap_or_default();
        let mut o = if let [peer] = peers.as_slice() {
            format!(
                "JOINED session={} role={rl} peer={peer} work={work}\n",
                sid(&s)
            )
        } else {
            format!(
                "JOINED session={} role={rl} peers={} work={work}\n",
                sid(&s),
                peers.join(",")
            )
        };
        o.push_str(&self.anchor_line());
        o.push('\n');
        for c in objects(s.get("companions")) {
            o.push_str(&companion_line(&c));
            o.push('\n');
        }
        for c in objects(s.get("absent_companions")) {
            o.push_str(&absent_line(&c));
            o.push('\n');
        }
        o.push_str(&format!(
            "TASK:\n{}\n",
            str_of(&s, "task").unwrap_or_default()
        ));
        Ok(Some(Out::ok(o)))
    }

    /// `health`: record this role's own health. Recovery is demonstrated by
    /// joining or posting, never declared: `ready` cannot clear a pause.
    ///
    /// # Errors
    /// An unknown status, an unreadable `resume_at`, or a declared recovery.
    pub fn health(
        &self,
        role: &str,
        status: &str,
        reason: Option<&str>,
        resume_at: Option<&str>,
    ) -> Result<Out, MeshError> {
        if !HEALTH.contains(&status) {
            return Err(refused(format!("invalid health status {status:?}")));
        }
        let resume = match resume_at.filter(|r| !r.is_empty()) {
            None => None,
            Some(r) => Some(normalise_ts(r).ok_or_else(|| {
                refused(format!(
                    "cannot read --resume-at {r:?}; use YYYY-MM-DDTHH:MM:SSZ or Unix seconds"
                ))
            })?),
        };
        {
            let _g = self.state_lock()?;
            let mut s = self.require(role, true)?;
            if status == "ready" && pauses(&s).contains_key(role) {
                return Err(refused(format!(
                    "{role} owns the recorded pause; it clears when that terminal runs `join`, or posts. It cannot be declared ready from here."
                )));
            }
            if status == "rate_limited" {
                let rec = json!({"role": role, "reason": reason.unwrap_or("rate_limit"),
                                 "resume_at": resume, "at": utc_now()});
                set_pause(&mut s, role, rec);
            }
            s.insert("updated_at".into(), json!(utc_now()));
            self.save(&s)?;
            self.set_health(&s, role, status, reason, resume.as_deref())?;
        }
        let tail = resume
            .map(|r| format!(" resume_at={r}"))
            .unwrap_or_default();
        Ok(Out::ok(format!(
            "HEALTH role={role} status={status}{tail}\n"
        )))
    }

    /// `status`: the session, its authority, health, delivery and waits.
    ///
    /// # Errors
    /// Unreadable mailbox state.
    pub fn status(&self) -> Result<Out, MeshError> {
        let mut o = String::new();
        let mut line = |l: String| {
            o.push_str(&l);
            o.push('\n');
        };
        let s = match self.active()? {
            Some(s) if status_of(&s) != "parked" => s,
            _ => {
                line("NO_ACTIVE_SESSION".into());
                for l in self.parked_lines()? {
                    line(l);
                }
                line(self.anchor_line());
                return Ok(Out {
                    code: 1,
                    stdout: o,
                    stderr: String::new(),
                });
            }
        };
        let units = objects(s.get("units"));
        let mut unit = unit_label(str_of(&s, "work_unit"), str_of(&s, "unit_id"));
        if !units.is_empty() {
            unit.push_str(&format!(" ({} closed)", units.len()));
        }
        line(format!(
            "SESSION {} status={} phase={} driver={} owner={} work={unit}",
            sid(&s),
            status_of(&s),
            str_of(&s, "phase").unwrap_or_default(),
            str_of(&s, "driver").unwrap_or_default(),
            str_of(&s, "owner").unwrap_or_default()
        ));
        if str_of(&s, "vcs") == Some("none") {
            line(format!(
                "VCS none base={}",
                str_of(&s, "base_head").unwrap_or("None")
            ));
        }
        for c in objects(s.get("companions")) {
            line(companion_line(&c));
        }
        for c in objects(s.get("absent_companions")) {
            line(absent_line(&c));
        }
        for u in &units {
            line(format!(
                "CLOSED_UNIT {} closed_at={} head={}",
                unit_label(str_of(u, "work_unit"), str_of(u, "unit_id")),
                str_of(u, "closed_at").unwrap_or("None"),
                str_of(u, "closed_head").unwrap_or("None")
            ));
        }
        let parts = participants(&s);
        if parts.len() > 2 {
            let (owner, req, adv) = authority(&s);
            line(authority_line(&owner, &req, &adv, None));
            for (r, pz) in pauses(&s) {
                line(pause_line(&s, &r, pz.as_object().unwrap_or(&Obj::new())));
            }
        }
        for r in &parts {
            let h = self.health_of(&s, r)?;
            line(format!(
                "{r}: {} updated={} resume_at={}",
                str_of(&h, "status").unwrap_or("unknown"),
                str_of(&h, "updated_at").unwrap_or("unknown"),
                str_of(&h, "resume_at")
                    .filter(|x| !x.is_empty())
                    .unwrap_or("-")
            ));
        }
        for r in &parts {
            for n in jsonl::invalid_lines(&self.mb.journal(sid(&s), r))? {
                line(format!("JOURNAL_INVALID {r} line={n}"));
            }
        }
        for l in self.endpoint_lines(&s)? {
            line(l);
        }
        let caps_rec = s
            .get("caps")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let caps = if caps_rec.is_empty() {
            "-".to_owned()
        } else {
            caps_rec
                .iter()
                .map(|(r, v)| format!("{r}:{}", strings(Some(v)).join("+")))
                .collect::<Vec<_>>()
                .join(",")
        };
        let mut un = Vec::new();
        for r in &parts {
            if schema(&s) == 1 {
                if let Some((a, b)) = self.unacked_range(&s, r)? {
                    un.push(format!("{r}=#{a}..#{b}"));
                }
            } else {
                let u = self.unacked_v2(&s, r)?;
                if !u.is_empty() {
                    let spans: Vec<String> = u
                        .iter()
                        .map(|(snd, a, b)| format!("{snd}:{a}..{snd}:{b}"))
                        .collect();
                    un.push(format!("{r}={}", spans.join(",")));
                }
            }
        }
        let tail = if un.is_empty() {
            String::new()
        } else {
            format!(" unacked {}", un.join(" "))
        };
        line(format!("DELIVERY mode={} caps={caps}{tail}", delivery(&s)));
        if schema(&s) == 2 && parts.len() > 2 {
            for (mid, e) in waiting_map(&s) {
                let e = e.as_object().cloned().unwrap_or_default();
                let dash = |v: Vec<String>| {
                    if v.is_empty() {
                        "-".to_owned()
                    } else {
                        v.join(",")
                    }
                };
                line(format!(
                    "WAITING {mid} {} from={} pending={} answered={}",
                    str_of(&e, "kind").unwrap_or("None"),
                    str_of(&e, "from").unwrap_or("None"),
                    dash(strings(e.get("pending"))),
                    dash(strings(e.get("answered_by")))
                ));
            }
        }
        for l in self.parked_lines()? {
            line(l);
        }
        line(self.anchor_line());
        Ok(Out::ok(o))
    }

    fn endpoint_lines(&self, s: &Obj) -> Result<Vec<String>, MeshError> {
        let mut out = Vec::new();
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        for r in participants(s) {
            let Some(ep) = self.read_obj(&self.mb.endpoint(sid(s), &r))? else {
                continue;
            };
            if ep.is_empty() {
                continue;
            }
            let gen = ep
                .get("generation")
                .map_or_else(|| "None".to_owned(), Value::to_string);
            if !ep.get("active").and_then(Value::as_bool).unwrap_or(false) {
                out.push(format!("ENDPOINT {r} unregistered generation={gen}"));
                continue;
            }
            let exp = str_of(&ep, "expires_at").filter(|x| !x.is_empty());
            let gone = exp.and_then(parse_utc).is_some_and(|t| t <= now);
            out.push(format!(
                "ENDPOINT {r} transport={} generation={gen} expires={}{}",
                str_of(&ep, "transport").unwrap_or("None"),
                exp.unwrap_or("-"),
                if gone { " EXPIRED" } else { "" }
            ));
        }
        Ok(out)
    }

    /// `PARKED` lines for every readable parked session; unreadable entries
    /// are skipped, as listing what can be resumed must not fail on them.
    fn parked_lines(&self) -> Result<Vec<String>, MeshError> {
        let base = self.mb.base().join("sessions");
        let Ok(rd) = std::fs::read_dir(&base) else {
            return Ok(Vec::new());
        };
        let mut names: Vec<String> = rd
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        names.sort();
        let mut out = Vec::new();
        for name in names {
            let d = base.join(&name);
            let (v1, v2) = (d.join(SESSION_V1), d.join(SESSION_V2));
            if v1.exists() && v2.exists() {
                continue;
            }
            let f = if v2.exists() { v2 } else { v1 };
            let Ok(Some(s)) = self.read_obj(&f) else {
                continue;
            };
            if sid(&s) != name || status_of(&s) != "parked" {
                continue;
            }
            out.push(format!(
                "PARKED {name} work={} from={} parked_at={}",
                str_of(&s, "work_unit").unwrap_or("None"),
                str_of(&s, "parked_from").unwrap_or("None"),
                str_of(&s, "parked_at").unwrap_or("None")
            ));
        }
        Ok(out)
    }

    /// `transcript`: every message of the active session, or of `session`.
    ///
    /// # Errors
    /// An unknown session, or a record from an unsupported protocol version.
    pub fn transcript(&self, session: Option<&str>) -> Result<Out, MeshError> {
        let s = match session {
            Some(want) => {
                let d = self.mb.base().join("sessions").join(want);
                let unknown = || refused(format!("unknown session {want:?}"));
                if want.is_empty()
                    || want.contains(['/', '\\'])
                    || want.starts_with('.')
                    || !d.is_dir()
                {
                    return Err(unknown());
                }
                let (v1, v2) = (d.join(SESSION_V1), d.join(SESSION_V2));
                if v1.exists() && v2.exists() {
                    return Err(MeshError::Corrupt(format!(
                        "ambiguous session directory {want:?}: holds both {SESSION_V1} and {SESSION_V2}"
                    )));
                }
                let s = self
                    .read_obj(if v2.exists() { &v2 } else { &v1 })?
                    .filter(|s| !s.is_empty())
                    .ok_or_else(unknown)?;
                check_protocol(&s, &format!("session {want}"))?;
                s
            }
            None => self.active()?.ok_or_else(|| refused("NO_ACTIVE_SESSION"))?,
        };
        let parts = participants(&s);
        let mut all = Vec::new();
        for r in &parts {
            all.extend(jsonl::records(&self.mb.journal(sid(&s), r))?);
        }
        let mut o = String::new();
        if parts.len() > 2 {
            for u in objects(s.get("units")) {
                if let Some(a) = u.get("authority").and_then(Value::as_object) {
                    o.push_str(&authority_line(
                        str_of(&u, "owner").unwrap_or("None"),
                        &strings(a.get("required_reviewers")),
                        &strings(a.get("advisers")),
                        Some(&unit_label(str_of(&u, "work_unit"), str_of(&u, "unit_id"))),
                    ));
                    o.push('\n');
                }
            }
            let (owner, req, adv) = authority(&s);
            let label = unit_label(str_of(&s, "work_unit"), str_of(&s, "unit_id"));
            o.push_str(&authority_line(&owner, &req, &adv, Some(&label)));
            o.push_str("\n\n");
        }
        all.sort_by(|a, b| {
            (
                str_of(a, "at").unwrap_or_default(),
                str_of(a, "role").unwrap_or_default(),
                seq_of(a),
            )
                .cmp(&(
                    str_of(b, "at").unwrap_or_default(),
                    str_of(b, "role").unwrap_or_default(),
                    seq_of(b),
                ))
        });
        for m in &all {
            let unit = match str_of(m, "work_unit").filter(|w| !w.is_empty()) {
                Some(w) => format!(" [{}]", unit_label(Some(w), str_of(m, "unit_id"))),
                None => String::new(),
            };
            let mk = if m.contains_key("msg_id") && !m["msg_id"].is_null() {
                super::render::route_marks(m)
            } else {
                String::new()
            };
            o.push_str(&format!(
                "## {} {} #{} {}{unit}{mk}\n{}\n\n",
                str_of(m, "at").unwrap_or_default(),
                str_of(m, "role").unwrap_or_default(),
                seq_of(m),
                str_of(m, "kind").unwrap_or_default(),
                str_of(m, "body").unwrap_or_default()
            ));
        }
        if status_of(&s) == "abandoned" {
            if let Some(why) = str_of(&s, "abandon_reason").filter(|w| !w.is_empty()) {
                let at = str_of(&s, "abandoned_at")
                    .or_else(|| str_of(&s, "updated_at"))
                    .unwrap_or("None");
                o.push_str(&format!("## {at} ABANDONED\n{why}\n\n"));
            }
        }
        Ok(Out::ok(o))
    }
}

fn objects(v: Option<&Value>) -> Vec<Obj> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_object().cloned()).collect())
        .unwrap_or_default()
}

/// A resume time as the protocol writes it: Unix seconds or the protocol
/// timestamp, normalised to the timestamp.
fn normalise_ts(v: &str) -> Option<String> {
    if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) {
        let t = time::OffsetDateTime::from_unix_timestamp(v.parse().ok()?).ok()?;
        return Some(crate::timefmt::format_utc(t));
    }
    parse_utc(v).map(|_| v.to_owned())
}
