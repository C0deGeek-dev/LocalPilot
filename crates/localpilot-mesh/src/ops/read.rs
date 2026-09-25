//! Reading: acknowledgements (spec C-4..C-9), the per-reader cursor, and
//! `watch`/`peek` delivery with its health and stale-wait notices.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::render::{route_marks, unit_label};
use super::{
    delivery, num, others, participants, refused, schema, seq_of, sid, str_of, strings, the_peer,
    visible, waiting_map, Mesh, Obj, DOWN,
};
use crate::error::MeshError;
use crate::fsio;
use crate::jsonl;
use crate::timefmt::parse_utc;

/// Keys of the scalar schema-2 cursor an older build wrote (mail).
const SCALAR_MAIL: &[&str] = &["peer_seq", "delivered_seq", "delivered_counts"];
/// Scalar health keys of that cursor, and their per-sender names.
const SCALAR_HEALTH: &[(&str, &str)] = &[
    ("peer_health_generation", "gen"),
    ("last_actionable_health", "last"),
    ("resume_seen", "resume_seen"),
];

/// How a `watch` or `peek` waits.
#[derive(Debug, Clone)]
pub struct WatchArgs {
    /// `false` for `peek`: look once and return.
    pub block: bool,
    /// Seconds before a blocking watch gives up; 0 waits for ever.
    pub timeout: u64,
    pub poll: Duration,
    /// Seconds after which an unanswered wait is reported stale.
    pub stale_after: i64,
    pub ack_through: Option<String>,
}

impl Default for WatchArgs {
    fn default() -> Self {
        Self {
            block: true,
            timeout: 0,
            poll: Duration::from_secs(1),
            stale_after: 900,
            ack_through: None,
        }
    }
}

/// A read cursor as `consume` left it, not yet written back.
pub(crate) struct Pending {
    path: std::path::PathBuf,
    cursor: Obj,
    entry: Obj,
    /// Schema 1 under print delivery writes the cursor unconditionally.
    always: bool,
    /// The session and reader, for merging with the stored cursor under the
    /// state lock before writing.
    s: Obj,
    role: String,
}

fn now_secs() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

fn obj_of(v: Option<&Value>) -> Obj {
    v.and_then(Value::as_object).cloned().unwrap_or_default()
}

fn counts_of(v: Option<&Value>) -> BTreeMap<String, i64> {
    obj_of(v).into_iter().map(|(k, v)| (k, num(&v))).collect()
}

fn key_num(k: &str) -> i64 {
    k.parse().unwrap_or(0)
}

/// The two sides of a cursor merge: the higher mark wins, counts are
/// maxed per message, and nothing at or below the acknowledged mark is kept.
fn merge_marks(mine: &mut Obj, stored: &Obj) {
    let acked = num_at(stored, "peer_seq").max(num_at(mine, "peer_seq"));
    let dl = num_at(stored, "delivered_seq").max(num_at(mine, "delivered_seq"));
    let ours = counts_of(mine.get("delivered_counts"));
    let mut counts = BTreeMap::new();
    for (k, v) in counts_of(stored.get("delivered_counts")) {
        counts.insert(k.clone(), v.max(ours.get(&k).copied().unwrap_or(0)));
    }
    for (k, v) in ours {
        counts.entry(k).or_insert(v);
    }
    counts.retain(|k, _| key_num(k) > acked);
    mine.insert("peer_seq".into(), json!(acked));
    mine.insert("delivered_seq".into(), json!(dl));
    mine.insert("delivered_counts".into(), json!(counts));
}

pub(crate) fn num_at(o: &Obj, k: &str) -> i64 {
    o.get(k).map_or(0, num)
}

fn fresh_sender() -> Value {
    json!({"peer_seq": 0, "delivered_seq": 0, "delivered_counts": {}})
}

/// A schema-2 cursor in its one shape, `{from, health, stale_marks}`; a
/// cursor in the older scalar shape is migrated the same way by every reader.
pub(crate) fn v2_cursor(s: &Obj, role: &str, raw: &Obj) -> Obj {
    let peers = others(s, role);
    let mut c = raw.clone();
    let mut frm = obj_of(c.get("from"));
    let mut hs = obj_of(c.get("health"));
    if !c.contains_key("stale_marks") {
        c.insert("stale_marks".into(), json!([]));
    }
    let legacy = SCALAR_MAIL.iter().any(|k| c.contains_key(*k))
        || SCALAR_HEALTH.iter().any(|(k, _)| c.contains_key(*k));
    if let (true, [peer]) = (legacy, peers.as_slice()) {
        let mut st = frm
            .get(peer)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_else(|| obj_of(Some(&fresh_sender())));
        st.insert(
            "peer_seq".into(),
            json!(num_at(&st, "peer_seq").max(num_at(&c, "peer_seq"))),
        );
        st.insert(
            "delivered_seq".into(),
            json!(num_at(&st, "delivered_seq").max(num_at(&c, "delivered_seq"))),
        );
        let mut merged = obj_of(c.get("delivered_counts"));
        merged.extend(obj_of(st.get("delivered_counts")));
        st.insert("delivered_counts".into(), Value::Object(merged));
        frm.insert(peer.clone(), Value::Object(st));
        let mut h = obj_of(hs.get(peer));
        for (old, new) in SCALAR_HEALTH {
            if let Some(v) = c.get(*old).filter(|v| !v.is_null()) {
                if h.get(*new).is_none_or(Value::is_null) {
                    h.insert((*new).into(), v.clone());
                }
            }
        }
        if let Some(g) = h.get("gen").cloned() {
            h.insert("gen".into(), json!(num(&g)));
        }
        hs.insert(peer.clone(), Value::Object(h));
    }
    for k in SCALAR_MAIL
        .iter()
        .copied()
        .chain(SCALAR_HEALTH.iter().map(|(k, _)| *k))
        .chain(["stale_wait_seq"])
    {
        c.remove(k);
    }
    for snd in &peers {
        let mut st = frm
            .get(snd)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for k in ["peer_seq", "delivered_seq"] {
            st.entry(k).or_insert(json!(0));
        }
        st.entry("delivered_counts").or_insert(json!({}));
        frm.insert(snd.clone(), Value::Object(st));
    }
    c.insert("from".into(), Value::Object(frm));
    c.insert("health".into(), Value::Object(hs));
    c
}

/// `{sender: seq}` from an `--through` / `--ack-through` value.
fn parse_ack(s: &Obj, role: &str, through: &str) -> Result<Vec<(String, i64)>, MeshError> {
    let peers = others(s, role);
    let t = through.trim();
    let positive =
        |x: &str| !x.is_empty() && !x.starts_with('0') && x.bytes().all(|b| b.is_ascii_digit());
    if positive(t) {
        let [peer] = peers.as_slice() else {
            return Err(refused(format!(
                "a bare --through names one sender; with {} participants write sender:N[,sender:N] (senders: {})",
                participants(s).len(),
                peers.join(", ")
            )));
        };
        return Ok(vec![(
            peer.clone(),
            t.parse()
                .map_err(|_| refused(format!("cannot read acknowledgement {t:?}")))?,
        )]);
    }
    if schema(s) == 1 {
        return Err(refused("sender:N acknowledgement needs an N-party session"));
    }
    let mut vec: Vec<(String, i64)> = Vec::new();
    for part in t.split(',') {
        let part = part.trim();
        let parsed = part.split_once(':').filter(|(snd, n)| {
            !snd.is_empty() && snd.bytes().all(|b| b.is_ascii_lowercase()) && positive(n)
        });
        let Some((snd, n)) = parsed else {
            return Err(refused(format!(
                "cannot read acknowledgement {part:?}; expected sender:N"
            )));
        };
        if snd == role {
            return Err(refused("a participant does not acknowledge its own mail"));
        }
        if !peers.iter().any(|p| p == snd) {
            return Err(refused(format!(
                "unknown sender {snd:?} (senders: {})",
                peers.join(", ")
            )));
        }
        if vec.iter().any(|(x, _)| x == snd) {
            return Err(refused(format!(
                "{snd} is named twice in the acknowledgement"
            )));
        }
        let n = n
            .parse()
            .map_err(|_| refused(format!("cannot read acknowledgement {part:?}")))?;
        vec.push((snd.to_owned(), n));
    }
    Ok(vec)
}

impl Mesh {
    pub(crate) fn read_obj(&self, p: &std::path::Path) -> Result<Option<Obj>, MeshError> {
        let v: Option<Value> = fsio::read_json(p, &self.mb.relative(p))?;
        Ok(v.map(|v| v.as_object().cloned().unwrap_or_default()))
    }

    fn cursor_of(&self, s: &Obj, role: &str) -> Result<Obj, MeshError> {
        Ok(self
            .read_obj(&self.mb.cursor(sid(s), role))?
            .unwrap_or_default())
    }

    /// Advance `role`'s acknowledged cursor(s); the caller holds the state
    /// lock. Only mail already presented can be acknowledged.
    pub(crate) fn apply_ack(
        &self,
        s: &Obj,
        role: &str,
        through: &str,
    ) -> Result<String, MeshError> {
        let vec = parse_ack(s, role, through)?;
        if schema(s) == 2 {
            return self.apply_ack_v2(s, role, &vec);
        }
        let n = vec.first().map_or(0, |(_, n)| *n);
        if delivery(s) != "ack" {
            return Ok(format!("ACK_NOT_NEEDED delivery={}", delivery(s)));
        }
        let p = self.mb.cursor(sid(s), role);
        let mut c = self.read_obj(&p)?.unwrap_or_default();
        let acked = num_at(&c, "peer_seq");
        let dl = acked.max(num_at(&c, "delivered_seq"));
        if n > dl {
            let tail = if dl > acked {
                format!("; valid range #{}..#{dl}", acked + 1)
            } else {
                "; nothing delivered is unacknowledged".to_owned()
            };
            return Err(refused(format!(
                "cannot ack #{n}: only #{dl} has been delivered to {role}{tail}"
            )));
        }
        if n <= acked {
            return Ok(format!("ACKED through={acked} (already)"));
        }
        c.insert("peer_seq".into(), json!(n));
        let mut counts = counts_of(c.get("delivered_counts"));
        counts.retain(|k, _| key_num(k) > n);
        c.insert("delivered_counts".into(), json!(counts));
        fsio::write_json(&p, &c)?;
        Ok(format!("ACKED through={n}"))
    }

    /// All-or-nothing: every component is validated before any is applied.
    fn apply_ack_v2(
        &self,
        s: &Obj,
        role: &str,
        vec: &[(String, i64)],
    ) -> Result<String, MeshError> {
        if delivery(s) != "ack" {
            return Ok(format!("ACK_NOT_NEEDED delivery={}", delivery(s)));
        }
        let p = self.mb.cursor(sid(s), role);
        let raw = self.read_obj(&p)?.unwrap_or_default();
        let mut c = v2_cursor(s, role, &raw);
        let frm = obj_of(c.get("from"));
        let mut plan = Vec::new();
        for (snd, n) in vec {
            let Some(m) = self.find_msg(s, &format!("{snd}:{n}"))? else {
                return Err(refused(format!(
                    "cannot ack {snd}:{n}: there is no such message"
                )));
            };
            if !visible(&m, role) {
                return Err(refused(format!(
                    "cannot ack {snd}:{n}: it is not mail addressed to {role}"
                )));
            }
            let st = obj_of(frm.get(snd));
            let acked = num_at(&st, "peer_seq");
            let dl = acked.max(num_at(&st, "delivered_seq"));
            if *n <= acked {
                continue;
            }
            if *n > dl {
                return Err(refused(format!(
                    "cannot ack {snd}:{n}: only {snd}:{dl} has been delivered to {role}"
                )));
            }
            plan.push((snd.clone(), *n));
        }
        if plan.is_empty() {
            if c != raw {
                fsio::write_json(&p, &c)?;
            }
            let all: Vec<String> = vec.iter().map(|(k, v)| format!("{k}:{v}")).collect();
            return Ok(format!("ACKED (already) {}", all.join(",")));
        }
        let mut frm = frm;
        for (snd, n) in &plan {
            let mut st = frm
                .get(snd)
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_else(|| obj_of(Some(&fresh_sender())));
            st.insert("peer_seq".into(), json!(n));
            let mut counts = counts_of(st.get("delivered_counts"));
            counts.retain(|k, _| key_num(k) > *n);
            st.insert("delivered_counts".into(), json!(counts));
            frm.insert(snd.clone(), Value::Object(st));
        }
        c.insert("from".into(), Value::Object(frm));
        fsio::write_json(&p, &c)?;
        let done: Vec<String> = plan.iter().map(|(k, v)| format!("{k}:{v}")).collect();
        Ok(format!("ACKED {}", done.join(",")))
    }

    /// `[(sender, first, last)]` of mail presented to `role` but unacknowledged.
    pub(crate) fn unacked_v2(
        &self,
        s: &Obj,
        role: &str,
    ) -> Result<Vec<(String, i64, i64)>, MeshError> {
        if delivery(s) != "ack" {
            return Ok(Vec::new());
        }
        let c = v2_cursor(s, role, &self.cursor_of(s, role)?);
        Ok(obj_of(c.get("from"))
            .iter()
            .filter_map(|(snd, st)| {
                let st = st.as_object()?;
                let (a, d) = (num_at(st, "peer_seq"), num_at(st, "delivered_seq"));
                (d > a).then(|| (snd.clone(), a + 1, d))
            })
            .collect())
    }

    /// `(first, last)` of schema-1 mail presented to `role` but unacknowledged.
    pub(crate) fn unacked_range(
        &self,
        s: &Obj,
        role: &str,
    ) -> Result<Option<(i64, i64)>, MeshError> {
        if delivery(s) != "ack" {
            return Ok(None);
        }
        let c = self.cursor_of(s, role)?;
        let (a, d) = (num_at(&c, "peer_seq"), num_at(&c, "delivered_seq"));
        Ok((d > a).then_some((a + 1, d)))
    }

    /// `ack`: acknowledge mail through the given point.
    ///
    /// # Errors
    /// A refusal names why; nothing is changed.
    pub fn ack(&self, role: &str, through: &str) -> Result<super::Out, MeshError> {
        let _g = self.state_lock()?;
        let s = self.require(role, true)?;
        let line = self.apply_ack(&s, role, through)?;
        Ok(super::Out::ok(format!("{line}\n")))
    }

    fn journal_after(&self, s: &Obj, sender: &str, after: i64) -> Result<Vec<Obj>, MeshError> {
        let mut out: Vec<Obj> = jsonl::records(&self.mb.journal(sid(s), sender))?
            .into_iter()
            .filter(|m| seq_of(m) > after)
            .collect();
        out.sort_by_key(seq_of);
        Ok(out)
    }

    /// The next thing to show `role`, and the cursor to commit once shown.
    /// The cursor is never written here: a delivery that fails must leave
    /// the mail unread.
    fn consume(
        &self,
        role: &str,
        stale: i64,
        expect_sid: Option<&str>,
    ) -> Result<(Option<String>, Pending), MeshError> {
        let s = self.require(role, true)?;
        let switched = |active: &str| {
            refused(format!(
                "SESSION_SWITCHED expected={} active={active}",
                expect_sid.unwrap_or_default()
            ))
        };
        if expect_sid.is_some_and(|e| e != sid(&s)) {
            return Err(switched(sid(&s)));
        }
        if schema(&s) == 2 {
            return self.consume_v2(&s, role, stale, expect_sid);
        }
        let acking = delivery(&s) == "ack";
        let path = self.mb.cursor(sid(&s), role);
        let mut c = self.read_obj(&path)?.unwrap_or_default();
        let entry = c.clone();
        let peer = the_peer(&s, role)?;
        let after = num_at(&c, "peer_seq");
        let msgs: Vec<Obj> = jsonl::records(&self.mb.journal(sid(&s), &peer))?
            .into_iter()
            .filter(|m| seq_of(m) > after)
            .collect();
        let pending = |c: Obj| Pending {
            path: path.clone(),
            cursor: c,
            entry: entry.clone(),
            always: !acking,
            s: s.clone(),
            role: role.to_owned(),
        };
        if !msgs.is_empty() {
            let mut n: BTreeMap<i64, i64> = BTreeMap::new();
            if acking {
                let mut counts = obj_of(c.get("delivered_counts"));
                for m in &msgs {
                    let k = seq_of(m).to_string();
                    let cnt = counts.get(&k).map_or(0, num) + 1;
                    counts.insert(k, json!(cnt));
                    n.insert(seq_of(m), cnt);
                }
                c.insert("delivered_counts".into(), Value::Object(counts));
                let top = n.keys().max().copied().unwrap_or(0);
                c.insert(
                    "delivered_seq".into(),
                    json!(num_at(&c, "delivered_seq").max(top)),
                );
            } else {
                let top = msgs.iter().map(seq_of).max().unwrap_or(0);
                c.insert("peer_seq".into(), json!(top));
            }
            let text = msgs
                .iter()
                .map(|m| block(m, n.get(&seq_of(m)).copied().unwrap_or(0), false))
                .collect::<Vec<_>>()
                .join("\n\n");
            return Ok((Some(text), pending(c)));
        }
        let h = self.health_of(&s, &peer)?;
        let gen = num_at(&h, "generation");
        let status = str_of(&h, "status").unwrap_or_default();
        if gen > num_at(&c, "peer_health_generation") {
            c.insert("peer_health_generation".into(), json!(gen));
            if DOWN.contains(&status) {
                c.insert("last_actionable_health".into(), json!(status));
                return Ok((Some(health_notice(&peer, &h)), pending(c)));
            }
            if status == "ready"
                && DOWN.contains(&str_of(&c, "last_actionable_health").unwrap_or_default())
            {
                c.insert("last_actionable_health".into(), json!("ready"));
                return Ok((Some(format!("PEER_HEALTH {peer} status=ready")), pending(c)));
            }
        }
        if let Some(notice) = resume_due(&peer, &h, &mut c) {
            return Ok((Some(notice), pending(c)));
        }
        let fresh = self.active()?.unwrap_or_else(|| s.clone());
        if expect_sid.is_some_and(|e| e != sid(&fresh)) {
            return Err(switched(sid(&fresh)));
        }
        if let Some(w) = fresh.get("waiting").and_then(Value::as_object) {
            let wseq = w.get("seq").cloned().unwrap_or(Value::Null);
            if str_of(w, "from_role") == Some(role)
                && c.get("stale_wait_seq").unwrap_or(&Value::Null) != &wseq
            {
                let since = str_of(w, "since").and_then(parse_utc).unwrap_or(i64::MAX);
                if now_secs().saturating_sub(since) >= stale && status != "rate_limited" {
                    c.insert("stale_wait_seq".into(), wseq.clone());
                    let text = format!(
                        "PEER_STALE {peer} waiting_on={}#{} last_health={}",
                        str_of(w, "kind").unwrap_or_default(),
                        num(&wseq),
                        str_of(&h, "updated_at").unwrap_or("unknown")
                    );
                    return Ok((Some(text), pending(c)));
                }
            }
        }
        Ok((None, pending(c)))
    }

    fn consume_v2(
        &self,
        s: &Obj,
        role: &str,
        stale: i64,
        expect_sid: Option<&str>,
    ) -> Result<(Option<String>, Pending), MeshError> {
        let acking = delivery(s) == "ack";
        let peers = others(s, role);
        let path = self.mb.cursor(sid(s), role);
        let raw = self.read_obj(&path)?.unwrap_or_default();
        let mut c = v2_cursor(s, role, &raw);
        let mut frm = obj_of(c.get("from"));
        let mut out: Vec<Obj> = Vec::new();
        let mut n: BTreeMap<(String, i64), i64> = BTreeMap::new();
        for snd in &peers {
            let mut st = obj_of(frm.get(snd));
            let mut counts = obj_of(st.get("delivered_counts"));
            let mut head = true;
            for m in self.journal_after(s, snd, num_at(&st, "peer_seq"))? {
                let seq = seq_of(&m);
                if !visible(&m, role) {
                    if head || !acking {
                        st.insert("peer_seq".into(), json!(seq));
                    }
                    continue;
                }
                if acking {
                    head = false;
                    let k = seq.to_string();
                    let cnt = counts.get(&k).map_or(0, num) + 1;
                    counts.insert(k, json!(cnt));
                    n.insert((snd.clone(), seq), cnt);
                    st.insert(
                        "delivered_seq".into(),
                        json!(num_at(&st, "delivered_seq").max(seq)),
                    );
                } else {
                    st.insert("peer_seq".into(), json!(seq));
                }
                out.push(m);
            }
            st.insert("delivered_counts".into(), Value::Object(counts));
            frm.insert(snd.clone(), Value::Object(st));
        }
        c.insert("from".into(), Value::Object(frm));
        let pending = |c: Obj| Pending {
            path: path.clone(),
            cursor: c,
            entry: raw.clone(),
            always: false,
            s: s.clone(),
            role: role.to_owned(),
        };
        if !out.is_empty() {
            out.sort_by(|a, b| {
                (str_of(a, "at"), str_of(a, "role"), seq_of(a)).cmp(&(
                    str_of(b, "at"),
                    str_of(b, "role"),
                    seq_of(b),
                ))
            });
            let marks = peers.len() > 1;
            let text = out
                .iter()
                .map(|m| {
                    let k = (str_of(m, "role").unwrap_or_default().to_owned(), seq_of(m));
                    block(m, n.get(&k).copied().unwrap_or(0), marks)
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            return Ok((Some(text), pending(c)));
        }
        let mut hs = obj_of(c.get("health"));
        for snd in &peers {
            let h = self.health_of(s, snd)?;
            let gen = num_at(&h, "generation");
            let mut hc = obj_of(hs.get(snd));
            let status = str_of(&h, "status").unwrap_or_default().to_owned();
            let mut notice = None;
            if gen > num_at(&hc, "gen") {
                hc.insert("gen".into(), json!(gen));
                if DOWN.contains(&status.as_str()) {
                    hc.insert("last".into(), json!(status));
                    notice = Some(health_notice(snd, &h));
                } else if status == "ready"
                    && DOWN.contains(&str_of(&hc, "last").unwrap_or_default())
                {
                    hc.insert("last".into(), json!("ready"));
                    notice = Some(format!("PEER_HEALTH {snd} status=ready"));
                }
            }
            if notice.is_none() {
                notice = resume_due_v2(snd, &h, &mut hc);
            }
            hs.insert(snd.clone(), Value::Object(hc));
            if let Some(text) = notice {
                c.insert("health".into(), Value::Object(hs));
                return Ok((Some(text), pending(c)));
            }
        }
        c.insert("health".into(), Value::Object(hs));
        let fresh = self.active()?.unwrap_or_else(|| s.clone());
        if let Some(e) = expect_sid.filter(|e| *e != sid(&fresh)) {
            return Err(refused(format!(
                "SESSION_SWITCHED expected={e} active={}",
                sid(&fresh)
            )));
        }
        let mut marks = strings(c.get("stale_marks"));
        for (mid, e) in waiting_map(&fresh) {
            let Some(e) = e.as_object() else { continue };
            if str_of(e, "from") != Some(role) {
                continue;
            }
            let since = str_of(e, "since").and_then(parse_utc).unwrap_or(i64::MAX);
            if now_secs().saturating_sub(since) < stale {
                continue;
            }
            for rcp in strings(e.get("pending")) {
                let key = format!("{mid}>{rcp}");
                let h = self.health_of(&fresh, &rcp)?;
                if marks.contains(&key) || str_of(&h, "status") == Some("rate_limited") {
                    continue;
                }
                marks.push(key);
                c.insert("stale_marks".into(), json!(marks));
                let text = format!(
                    "PEER_STALE {rcp} waiting_on={}#{mid} last_health={}",
                    str_of(e, "kind").unwrap_or_default(),
                    str_of(&h, "updated_at").unwrap_or("unknown")
                );
                return Ok((Some(text), pending(c)));
            }
        }
        Ok((None, pending(c)))
    }

    /// Write back a cursor `consume` produced.
    fn commit(&self, p: Pending) -> Result<(), MeshError> {
        if p.always {
            return fsio::write_json(&p.path, &p.cursor);
        }
        if p.cursor == p.entry {
            return Ok(());
        }
        let _g = self.state_lock()?;
        let stored = self.read_obj(&p.path)?.unwrap_or_default();
        let mut c = p.cursor;
        if schema(&p.s) == 1 {
            merge_marks(&mut c, &stored);
        } else {
            {
                let sf = obj_of(stored.get("from"));
                let mut frm = obj_of(c.get("from"));
                for (snd, st) in &mut frm {
                    if let Value::Object(st) = st {
                        merge_marks(st, &obj_of(sf.get(snd)));
                    }
                }
                c.insert("from".into(), Value::Object(frm));
                // Health and stale markers from the stored cursor survive
                // this older snapshot: the higher generation wins.
                let sh = obj_of(v2_cursor(&p.s, &p.role, &stored).get("health"));
                let mut mine = obj_of(c.get("health"));
                for (snd, o) in sh {
                    let o = o.as_object().cloned().unwrap_or_default();
                    match mine.get_mut(&snd).and_then(Value::as_object_mut) {
                        Some(m) if num_at(&o, "gen") < num_at(m, "gen") => {}
                        Some(m) if num_at(&o, "gen") == num_at(m, "gen") => {
                            for (k, v) in o {
                                if m.get(&k).is_none_or(Value::is_null) {
                                    m.insert(k, v);
                                }
                            }
                        }
                        _ => {
                            mine.insert(snd, Value::Object(o));
                        }
                    }
                }
                c.insert("health".into(), Value::Object(mine));
                let mut marks = strings(c.get("stale_marks"));
                marks.extend(strings(stored.get("stale_marks")));
                marks.sort();
                marks.dedup();
                c.insert("stale_marks".into(), json!(marks));
            }
        }
        fsio::write_json(&p.path, &c)
    }

    /// `watch` (blocking) and `peek` (one look). `emit` prints one delivery;
    /// the cursor is committed only after it returns successfully.
    ///
    /// # Errors
    /// A refusal, a switched session, or a failed `emit`.
    pub fn watch(
        &self,
        role: &str,
        a: &WatchArgs,
        emit: &mut dyn FnMut(&str) -> std::io::Result<()>,
    ) -> Result<u8, MeshError> {
        let end =
            (a.block && a.timeout > 0).then(|| Instant::now() + Duration::from_secs(a.timeout));
        // A blocking watch belongs to the session it started on.
        let start_sid = self.active()?.map(|s| sid(&s).to_owned());
        if let Some(t) = &a.ack_through {
            let _g = self.state_lock()?;
            let s = self.require(role, true)?;
            self.apply_ack(&s, role, t)?;
        }
        loop {
            let (text, pending) = self.consume(role, a.stale_after, start_sid.as_deref())?;
            if let Some(text) = text {
                emit(&text).map_err(|e| MeshError::io(std::path::Path::new("<stdout>"), e))?;
                self.commit(pending)?;
                return Ok(0);
            }
            self.commit(pending)?;
            if !a.block {
                return Ok(0);
            }
            if end.is_some_and(|e| Instant::now() >= e) {
                return Ok(1);
            }
            std::thread::sleep(a.poll);
        }
    }
}

fn health_notice(who: &str, h: &Obj) -> String {
    format!(
        "PEER_HEALTH {who} status={} resume_at={} reason={}",
        str_of(h, "status").unwrap_or_default(),
        str_of(h, "resume_at").unwrap_or("unknown"),
        str_of(h, "reason").unwrap_or("unspecified")
    )
}

fn resume_due_at(who: &str, h: &Obj, seen: Option<&Value>) -> Option<String> {
    if str_of(h, "status") != Some("rate_limited") {
        return None;
    }
    let at = str_of(h, "resume_at")?;
    let due = parse_utc(at)?;
    (now_secs() >= due && seen.and_then(Value::as_str) != Some(at))
        .then(|| format!("PEER_RESUME_DUE {who} reset={at} (retry due; access not proven)"))
}

fn resume_due(who: &str, h: &Obj, c: &mut Obj) -> Option<String> {
    let text = resume_due_at(who, h, c.get("resume_seen"))?;
    c.insert(
        "resume_seen".into(),
        h.get("resume_at").cloned().unwrap_or(Value::Null),
    );
    Some(text)
}

fn resume_due_v2(who: &str, h: &Obj, hc: &mut Obj) -> Option<String> {
    let text = resume_due_at(who, h, hc.get("resume_seen"))?;
    hc.insert(
        "resume_seen".into(),
        h.get("resume_at").cloned().unwrap_or(Value::Null),
    );
    Some(text)
}

/// One delivered message: its heading, then its body.
fn block(m: &Obj, count: i64, marks: bool) -> String {
    let unit = match str_of(m, "work_unit").filter(|w| !w.is_empty()) {
        Some(w) => format!(" [{}]", unit_label(Some(w), str_of(m, "unit_id"))),
        None => String::new(),
    };
    let mk = if marks { route_marks(m) } else { String::new() };
    let reply = if m
        .get("expect_reply")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        " REPLY_REQUIRED"
    } else {
        ""
    };
    let again = if count > 1 {
        format!(" REDELIVERED n={count}")
    } else {
        String::new()
    };
    let head = format!(
        "PEER {} #{} {}{unit}{mk}{reply}{again}",
        str_of(m, "role").unwrap_or_default(),
        seq_of(m),
        str_of(m, "kind").unwrap_or_default()
    );
    match str_of(m, "body").filter(|b| !b.is_empty()) {
        Some(b) => format!("{head}\n{b}"),
        None => head,
    }
}
