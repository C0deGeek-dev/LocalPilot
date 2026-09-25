//! Posting: addressing (spec A-1..A-6), the append (M-1, M-2, J-1) and the
//! session update that follows it (W-1, W-2).

use serde_json::{json, Value};

use super::{
    clear_pause, num, others, participants, pauses, refused, schema, seq_of, sid, str_of, strings,
    the_peer, waiting_map, Mesh, Obj, Out, BROADCAST_KINDS, FORWARD_TTL, KINDS, MAX_BODY,
};
use crate::error::MeshError;
use crate::jsonl;
use crate::lock::Lock;
use crate::timefmt::utc_now;
use crate::PROTOCOL;

/// What `post` was asked to send.
#[derive(Debug, Clone, Default)]
pub struct PostArgs {
    pub kind: String,
    pub body: String,
    pub expect_reply: bool,
    /// Comma-separated recipients, as given.
    pub to: Option<String>,
    pub reply_to: Option<String>,
    pub broadcast: bool,
    pub forward: bool,
    /// `N` or `sender:N[,sender:N]`, acknowledged before the post.
    pub ack_through: Option<String>,
}

/// The directed-mail fields a schema-2 message carries.
struct Route {
    to: Vec<String>,
    broadcast: bool,
    reply_to: Option<String>,
    thread_id: Option<String>,
    route_trace: Vec<String>,
    ttl: Value,
    forward: bool,
}

fn check_recipients(s: &Obj, role: &str, named: &[String]) -> Result<(), MeshError> {
    let parts = participants(s);
    for x in named {
        if !parts.contains(x) {
            return Err(refused(format!(
                "unknown recipient {x:?} (participants: {})",
                parts.join(", ")
            )));
        }
        if x == role {
            return Err(refused("a participant cannot address itself"));
        }
    }
    let mut seen = named.to_vec();
    seen.sort();
    seen.dedup();
    if seen.len() != named.len() {
        return Err(refused("--to names a recipient twice"));
    }
    Ok(())
}

impl Mesh {
    /// Recipients and lineage for a post, validated on snapshot `s`. Schema 1
    /// is the historic pair: its peer is implicit and directed-mail flags are
    /// refused rather than ignored.
    fn address(
        &self,
        s: &Obj,
        role: &str,
        a: &PostArgs,
        internal: bool,
    ) -> Result<Option<Route>, MeshError> {
        let kind = a.kind.as_str();
        if schema(s) == 1 {
            if a.to.is_some() || a.reply_to.is_some() || a.broadcast || a.forward {
                return Err(refused(
                    "--to, --reply-to and --broadcast need an N-party session (start it with --with)",
                ));
            }
            the_peer(s, role)?;
            return Ok(None);
        }
        let reply_to = a.reply_to.clone();
        if a.broadcast && a.forward {
            return Err(refused(
                "--forward and --broadcast are exclusive: a forward goes to named recipients",
            ));
        }
        if a.broadcast {
            if reply_to.is_some() {
                return Err(refused("--broadcast and --reply-to are exclusive; a threaded ESCALATE goes to the thread origin"));
            }
            if !internal && !BROADCAST_KINDS.contains(&kind) {
                return Err(refused(format!(
                    "--broadcast is allowed only for {}; {kind} notices come from their own commands",
                    BROADCAST_KINDS.join(", ")
                )));
            }
            if a.expect_reply {
                return Err(refused(
                    "--broadcast cannot --expect-reply: a notice must not create a wait",
                ));
            }
            if a.to.is_some() {
                return Err(refused("--broadcast and --to are exclusive"));
            }
            return Ok(Some(Route {
                to: others(s, role),
                broadcast: true,
                reply_to: None,
                thread_id: None,
                route_trace: vec![role.to_owned()],
                ttl: Value::Null,
                forward: false,
            }));
        }
        let mut named: Vec<String> =
            a.to.as_deref()
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .map(str::to_owned)
                .collect();
        let not_visible = |r: &str| {
            refused(format!(
                "--reply-to {r}: no such message visible to {role} in this session"
            ))
        };
        if a.forward {
            let Some(r) = reply_to else {
                return Err(refused(
                    "--forward needs --reply-to <msg_id> (the message being forwarded)",
                ));
            };
            if named.is_empty() {
                return Err(refused(
                    "--forward needs --to (who the request goes to next)",
                ));
            }
            let mref = self
                .find_visible(s, &r, role)?
                .ok_or_else(|| not_visible(&r))?;
            check_recipients(s, role, &named)?;
            let author = str_of(&mref, "role").unwrap_or_default().to_owned();
            let mut route = strings(mref.get("route_trace"));
            if route.is_empty() {
                route = vec![author];
            }
            if route.iter().any(|x| x == role) {
                return Err(refused(format!(
                    "{role} is already on this request's route ({}); forward only a request you received. Reply instead, or ESCALATE to the thread origin",
                    route.join(" -> ")
                )));
            }
            route.push(role.to_owned());
            let ttl = match mref.get("ttl") {
                None | Some(Value::Null) => FORWARD_TTL,
                Some(v) => num(v),
            };
            if ttl <= 0 {
                return Err(refused(format!(
                    "{r} may not be forwarded again (ttl exhausted); reply to it, or ESCALATE to the thread origin"
                )));
            }
            let looped: Vec<&str> = named
                .iter()
                .filter(|x| route.contains(x))
                .map(String::as_str)
                .collect();
            if !looped.is_empty() {
                return Err(refused(format!(
                    "{} already on this request's route ({}); a forward cannot loop. Reply instead, or ESCALATE to the thread origin",
                    looped.join(", "),
                    route.join(" -> ")
                )));
            }
            let thread = str_of(&mref, "thread_id").map_or_else(|| r.clone(), str::to_owned);
            return Ok(Some(Route {
                to: named,
                broadcast: false,
                reply_to: Some(r),
                thread_id: Some(thread),
                route_trace: route,
                ttl: json!(ttl - 1),
                forward: true,
            }));
        }
        check_recipients(s, role, &named)?;
        if let Some(r) = reply_to {
            let mref = self
                .find_visible(s, &r, role)?
                .ok_or_else(|| not_visible(&r))?;
            let author = str_of(&mref, "role").unwrap_or_default().to_owned();
            let thread = str_of(&mref, "thread_id").map_or_else(|| r.clone(), str::to_owned);
            if kind == "ESCALATE" && named.is_empty() {
                let origin = thread.split(':').next().unwrap_or_default().to_owned();
                named = vec![if origin == role {
                    author.clone()
                } else {
                    origin
                }];
            }
            if named.is_empty() {
                named = vec![author.clone()];
            }
            if kind != "ESCALATE" && named != [author.clone()] {
                return Err(refused(format!(
                    "a reply goes to the author of {r} ({author}); reaching anyone else is a forward"
                )));
            }
            if author == role && kind != "ESCALATE" {
                return Err(refused("a reply to your own message has no recipient"));
            }
            check_recipients(s, role, &named)?;
            let mut trace = strings(mref.get("route_trace"));
            if trace.is_empty() {
                trace = vec![author];
            }
            return Ok(Some(Route {
                to: named,
                broadcast: false,
                reply_to: Some(r),
                thread_id: Some(thread),
                route_trace: trace,
                ttl: mref.get("ttl").cloned().unwrap_or(Value::Null),
                forward: false,
            }));
        }
        if named.is_empty() {
            let o = others(s, role);
            if o.len() != 1 {
                return Err(refused(format!(
                    "--to is required with {} participants",
                    participants(s).len()
                )));
            }
            named = o;
        }
        check_recipients(s, role, &named)?;
        Ok(Some(Route {
            to: named,
            broadcast: false,
            reply_to: None,
            thread_id: None,
            route_trace: vec![role.to_owned()],
            ttl: Value::Null,
            forward: false,
        }))
    }

    /// Append one message to `role`'s journal and update the session.
    ///
    /// `expect_sid` pins the post to the session a caller already resolved;
    /// `internal` lets a command's own notice (a HELLO) be broadcast.
    pub(crate) fn post_message(
        &self,
        role: &str,
        a: &PostArgs,
        expect_sid: Option<&str>,
        internal: bool,
    ) -> Result<Obj, MeshError> {
        let s = self.require(role, true)?;
        if let Some(want) = expect_sid {
            if sid(&s) != want {
                return Err(refused(format!(
                    "session changed under this command (expected {want}, active {})",
                    sid(&s)
                )));
            }
        }
        let kind = a.kind.as_str();
        if !KINDS.contains(&kind) {
            return Err(refused("invalid kind"));
        }
        let route = self.address(&s, role, a, internal)?;
        let body = a.body.trim();
        if body.is_empty() {
            return Err(refused("refusing to post an empty message; a body that failed to render is worse than no post, because the peer treats it as a real turn"));
        }
        if a.body.chars().count() > MAX_BODY {
            return Err(refused(
                "message too large; reference a repo artifact instead",
            ));
        }
        // A post is proof of life: it clears the poster's own recorded pause,
        // provided nobody replaced that pause since this command read it.
        let entry_pause = pauses(&s).get(role).cloned();
        if let Some(ep) = &entry_pause {
            let _g = self.state_lock()?;
            if let Some(mut fresh) = self.active()? {
                if sid(&fresh) == sid(&s) && pauses(&fresh).get(role) == Some(ep) {
                    clear_pause(&mut fresh, role);
                    fresh.insert("updated_at".into(), json!(utc_now()));
                    self.save(&fresh)?;
                    self.set_health(&fresh, role, "ready", None, None)?;
                }
            }
        }
        let jp = self.mb.journal(sid(&s), role);
        let m = {
            let _g = Lock::acquire(&self.mb.role_lock(sid(&s), role))?;
            let last = self
                .read_obj(&self.mb.latest(sid(&s), role))?
                .map_or(0, |l| seq_of(&l));
            let head = jsonl::records(&jp)?.last().map_or(0, seq_of);
            let seq = last.max(head) + 1;
            let at = utc_now();
            let mut m = Obj::new();
            m.insert("seq".into(), json!(seq));
            m.insert("at".into(), json!(at));
            m.insert("role".into(), json!(role));
            m.insert("kind".into(), json!(kind));
            m.insert(
                "work_unit".into(),
                s.get("work_unit").cloned().unwrap_or(Value::Null),
            );
            m.insert(
                "unit_id".into(),
                s.get("unit_id").cloned().unwrap_or(Value::Null),
            );
            m.insert("expect_reply".into(), json!(a.expect_reply));
            m.insert("body".into(), json!(body));
            if schema(&s) == 2 {
                m.insert(
                    "owner".into(),
                    s.get("owner").cloned().unwrap_or(Value::Null),
                );
            }
            if seq == 1 {
                m.insert("protocol".into(), json!(PROTOCOL));
            }
            if let Some(r) = &route {
                let mid = format!("{role}:{seq}");
                m.insert("msg_id".into(), json!(mid));
                m.insert("to".into(), json!(r.to));
                m.insert("reply_to".into(), json!(r.reply_to));
                m.insert("broadcast".into(), json!(r.broadcast));
                m.insert(
                    "thread_id".into(),
                    json!(r.thread_id.clone().unwrap_or(mid)),
                );
                m.insert("route_trace".into(), json!(r.route_trace));
                m.insert("ttl".into(), r.ttl.clone());
                m.insert("forward".into(), json!(r.forward));
            }
            jsonl::append(&jp, &m)?;
            crate::fsio::write_json(&self.mb.latest(sid(&s), role), &m)?;
            m
        };
        self.after_post(&s, role, &m, entry_pause.is_some(), a.expect_reply)?;
        Ok(m)
    }

    /// The session update a post makes: recovered health, and the waits it
    /// answers or opens (spec W-1, W-2).
    fn after_post(
        &self,
        s: &Obj,
        role: &str,
        m: &Obj,
        had_pause: bool,
        expect: bool,
    ) -> Result<(), MeshError> {
        let _g = self.state_lock()?;
        let Some(mut fresh) = self.active()? else {
            return Ok(());
        };
        if sid(&fresh) != sid(s) {
            return Ok(());
        }
        if !had_pause && !pauses(&fresh).contains_key(role) {
            let h = self.health_of(&fresh, role)?;
            if matches!(
                str_of(&h, "status"),
                Some("rate_limited" | "paused" | "offline")
            ) {
                self.set_health(&fresh, role, "ready", None, None)?;
            }
        }
        if fresh.get("unit_id") == m.get("unit_id") {
            let seq = seq_of(m);
            let at = m.get("at").cloned().unwrap_or(Value::Null);
            let kind = str_of(m, "kind").unwrap_or_default();
            if schema(&fresh) == 1 {
                let answered = fresh
                    .get("waiting")
                    .and_then(Value::as_object)
                    .is_some_and(|w| str_of(w, "for_role") == Some(role));
                if answered {
                    fresh.insert("waiting".into(), Value::Null);
                }
                if expect {
                    let peer = the_peer(&fresh, role)?;
                    fresh.insert(
                        "waiting".into(),
                        json!({"from_role": role, "for_role": peer, "seq": seq, "kind": kind, "since": at}),
                    );
                }
            } else {
                let mut wm = waiting_map(&fresh);
                if let Some(rt) = str_of(m, "reply_to") {
                    let fwd = m.get("forward").and_then(Value::as_bool).unwrap_or(false);
                    let mut done = false;
                    if let Some(Value::Object(e)) = wm.get_mut(rt) {
                        let pending = strings(e.get("pending"));
                        if !fwd && kind != "ESCALATE" && pending.iter().any(|x| x == role) {
                            let rest: Vec<String> =
                                pending.into_iter().filter(|x| x != role).collect();
                            let mut by = strings(e.get("answered_by"));
                            by.push(role.to_owned());
                            done = rest.is_empty();
                            e.insert("pending".into(), json!(rest));
                            e.insert("answered_by".into(), json!(by));
                        }
                    }
                    if done {
                        wm.remove(rt);
                    }
                }
                if expect {
                    let mid = str_of(m, "msg_id").unwrap_or_default().to_owned();
                    wm.insert(
                        mid,
                        json!({"from": role, "kind": kind, "since": at, "pending": m.get("to"), "answered_by": []}),
                    );
                }
                fresh.insert(
                    "waiting".into(),
                    if wm.is_empty() {
                        Value::Null
                    } else {
                        Value::Object(wm)
                    },
                );
            }
        }
        fresh.insert("updated_at".into(), json!(utc_now()));
        self.save(&fresh)
    }

    /// `post`: acknowledge first when asked, then post; a reminder of mail
    /// still unacknowledged goes to stderr.
    ///
    /// # Errors
    /// A refusal ([`MeshError::Refused`]) leaves the mailbox as it was.
    pub fn post(&self, role: &str, a: &PostArgs) -> Result<Out, MeshError> {
        // Addressing and the body are checked before the acknowledgement, so
        // a refused post leaves no acknowledgement behind.
        if let Some(s0) = self.active()? {
            if participants(&s0).iter().any(|p| p == role) {
                self.address(&s0, role, a, false)?;
            }
        }
        if a.body.trim().is_empty() {
            return Err(refused("refusing to post an empty message; a body that failed to render is worse than no post, because the peer treats it as a real turn"));
        }
        if let Some(t) = &a.ack_through {
            let _g = self.state_lock()?;
            let s = self.require(role, true)?;
            self.apply_ack(&s, role, t)?;
        }
        self.post_message(role, a, None, false)?;
        let mut out = Out::code(0);
        if let Some(s) = self.active()? {
            if schema(&s) == 1 {
                if let Some((x, y)) = self.unacked_range(&s, role)? {
                    out.stderr = format!(
                        "UNACKED peer #{x}..#{y} (post --ack-through N or `ack --through N` once read)\n"
                    );
                }
            } else {
                let u = self.unacked_v2(&s, role)?;
                if !u.is_empty() {
                    let spans: Vec<String> = u
                        .iter()
                        .map(|(snd, x, y)| format!("{snd}:{x}..{snd}:{y}"))
                        .collect();
                    out.stderr = format!(
                        "UNACKED {} (post --ack-through sender:N[,sender:N] or `ack --through ...` once read)\n",
                        spans.join(",")
                    );
                }
            }
        }
        Ok(out)
    }
}
