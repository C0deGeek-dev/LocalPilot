//! Usage, explicit quota transitions, and opt-in advisory context (T-1..T-3).
use super::*;
use crate::session::check_protocol;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Incremental usage for one turn. Missing cost or quota stays unknown.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageReport {
    pub report_id: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cost_microusd: Option<u64>,
    pub limit_percent: Option<u8>,
}

pub(super) fn usage_lines(s: &Obj) -> Vec<String> {
    #[derive(Default)]
    struct Group {
        tokens: u128,
        cost: u128,
        known: usize,
        reports: usize,
        limit: Option<u64>,
    }
    let mut groups: BTreeMap<(String, String, String), Group> = BTreeMap::new();
    for row in s
        .get("usage")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(r) = row.as_object() else { continue };
        let key = (
            str_of(r, "role").unwrap_or_default().to_owned(),
            str_of(r, "unit_id").unwrap_or_default().to_owned(),
            str_of(r, "source").unwrap_or_default().to_owned(),
        );
        let g = groups.entry(key).or_default();
        for k in [
            "input_tokens",
            "output_tokens",
            "cache_creation_input_tokens",
            "cache_read_input_tokens",
        ] {
            g.tokens += u128::from(r.get(k).and_then(Value::as_u64).unwrap_or(0));
        }
        g.reports += 1;
        if let Some(v) = r.get("cost_microusd").and_then(Value::as_u64) {
            g.cost += u128::from(v);
            g.known += 1;
        }
        if let Some(v) = r.get("limit_percent").and_then(Value::as_u64) {
            g.limit = Some(v);
        }
    }
    groups.into_iter().map(|((role,unit,source),g)| format!("USAGE role={role} unit={unit} source={source} tokens={} cost_microusd={} cost_coverage={}/{} limit_percent={} reports={} coverage=partial",g.tokens,if g.known==0 {"unknown".to_owned()} else {g.cost.to_string()},g.known,g.reports,g.limit.map_or_else(||"unknown".to_owned(),|v|v.to_string()),g.reports)).collect()
}

impl Mesh {
    /// Read the usage ledger for the active or named session (T-1).
    pub fn usage(&self, session_id: Option<&str>) -> Result<Out, MeshError> {
        let s = if let Some(id) = session_id {
            if id.is_empty() || id.contains(['/', '\\']) || id.starts_with('.') {
                return Err(refused(format!("unknown session {id:?}")));
            }
            let d = self.mb.session_dir(id);
            let v1 = d.join(SESSION_V1);
            let v2 = d.join(SESSION_V2);
            if v1.exists() && v2.exists() {
                return Err(refused("ambiguous session directory"));
            }
            let s = self
                .read_obj(if v2.exists() { &v2 } else { &v1 })?
                .ok_or_else(|| refused(format!("unknown session {id:?}")))?;
            check_protocol(&s, &format!("session {id}"))?;
            s
        } else {
            self.active()?.ok_or_else(|| refused("NO_ACTIVE_SESSION"))?
        };
        let lines = usage_lines(&s);
        Ok(Out::ok(if lines.is_empty() {
            "USAGE coverage=none\n".into()
        } else {
            format!("{}\n", lines.join("\n"))
        }))
    }

    /// Pin an engine turn's ledger scope before running its model.
    pub fn usage_scope(&self, role: &str) -> Result<(String, String), MeshError> {
        let s = self.require(role, true)?;
        Ok((
            sid(&s).to_owned(),
            str_of(&s, "unit_id").unwrap_or_default().to_owned(),
        ))
    }

    /// Append incremental usage idempotently. An engine may pin the scope.
    pub fn record_usage(
        &self,
        role: &str,
        report: &UsageReport,
        source: &str,
        expected: Option<&(String, String)>,
    ) -> Result<Out, MeshError> {
        if report.report_id.is_empty()
            || report.report_id.len() > 128
            || !report
                .report_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b))
        {
            return Err(refused("invalid report id"));
        }
        if report.limit_percent.is_some_and(|v| v > 100) {
            return Err(refused("limit percent must be 0..100"));
        }
        if !["reported", "engine"].contains(&source) {
            return Err(refused("invalid usage source"));
        }
        let _lock = self.state_lock()?;
        let mut s = self.require(role, true)?;
        if expected
            .is_some_and(|(id, unit)| id != sid(&s) || Some(unit.as_str()) != str_of(&s, "unit_id"))
        {
            return Err(refused("usage scope changed during the turn"));
        }
        let mut rec = serde_json::to_value(report)
            .map_err(|e| refused(e.to_string()))?
            .as_object()
            .cloned()
            .ok_or_else(|| refused("invalid report"))?;
        rec.insert("role".into(), json!(role));
        rec.insert(
            "unit_id".into(),
            s.get("unit_id").cloned().unwrap_or(Value::Null),
        );
        rec.insert("source".into(), json!(source));
        let mut rows = s
            .get("usage")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if let Some(old) = rows.iter().filter_map(Value::as_object).find(|r| {
            str_of(r, "role") == Some(role)
                && str_of(r, "report_id") == Some(report.report_id.as_str())
        }) {
            let mut old = old.clone();
            old.remove("at");
            if old != rec {
                return Err(refused("usage report id reused with different content"));
            }
        } else {
            rec.insert("at".into(), json!(utc_now()));
            rows.push(Value::Object(rec));
            s.insert("usage".into(), json!(rows));
            s.insert("updated_at".into(), json!(utc_now()));
            self.save(&s)?;
        }
        Ok(Out::ok(format!(
            "USAGE_RECORDED role={role} report_id={}\n",
            report.report_id
        )))
    }

    fn healthy(&self, s: &Obj, role: &str) -> Result<bool, MeshError> {
        Ok(!pauses(s).contains_key(role)
            && str_of(&self.health_of(s, role)?, "status") == Some("ready"))
    }

    /// Pre-authorize or revoke a named quota takeover while healthy (T-2).
    pub fn takeover_authorize(
        &self,
        role: &str,
        to: &str,
        duty: &str,
        revoke: bool,
    ) -> Result<Out, MeshError> {
        let _lock = self.state_lock()?;
        let mut s = self.require(role, false)?;
        let (owner, req, adv) = authority(&s);
        if schema(&s) != 2 {
            return Err(refused(
                "takeover needs a three-party session with an independent reviewer",
            ));
        }
        if !self.healthy(&s, role)? {
            return Err(refused("only a healthy duty holder may authorize"));
        }
        if !participants(&s).iter().any(|r| r == to) || to == role || !self.healthy(&s, to)? {
            return Err(refused("successor must be another healthy participant"));
        }
        if !["owner", "reviewer"].contains(&duty)
            || (duty == "owner" && owner != role)
            || (duty == "reviewer" && !req.iter().any(|r| r == role))
        {
            return Err(refused("role does not hold that duty"));
        }
        if duty == "reviewer" && !adv.iter().any(|r| r == to) {
            return Err(refused("reviewer successor must be an adviser"));
        }
        let mut rows: Vec<Value> = s
            .get("takeover_authorizations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|x| {
                x.get("from").and_then(Value::as_str) != Some(role)
                    || x.get("duty").and_then(Value::as_str) != Some(duty)
            })
            .cloned()
            .collect();
        if !revoke {
            rows.push(json!({"from":role,"to":to,"duty":duty,"session_id":sid(&s),"unit_id":s.get("unit_id"),"epoch":s.get("ownership_epoch"),"at":utc_now()}));
        }
        s.insert("takeover_authorizations".into(), json!(rows));
        require_takeover(&mut s);
        super::unit::add_event(
            &mut s,
            if revoke {
                "takeover-revoked"
            } else {
                "takeover-authorized"
            },
            role,
            None,
            Some(json!({"to":to,"duty":duty})),
        );
        s.insert("updated_at".into(), json!(utc_now()));
        self.save(&s)?;
        Ok(Out::ok(format!(
            "TAKEOVER_{} from={role} to={to} duty={duty}\n",
            if revoke { "REVOKED" } else { "AUTHORIZED" }
        )))
    }

    /// Apply a current authorization only after its holder pauses (T-2).
    pub fn takeover(&self, role: &str, from: &str, duty: &str) -> Result<Out, MeshError> {
        let _lock = self.state_lock()?;
        let mut s = self.require(role, true)?;
        let valid = s
            .get("takeover_authorizations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|x| {
                x.get("from").and_then(Value::as_str) == Some(from)
                    && x.get("to").and_then(Value::as_str) == Some(role)
                    && x.get("duty").and_then(Value::as_str) == Some(duty)
                    && x.get("session_id").and_then(Value::as_str) == Some(sid(&s))
                    && x.get("unit_id") == s.get("unit_id")
                    && x.get("epoch") == s.get("ownership_epoch")
            });
        if !valid {
            return Err(refused("no current authorization for this takeover"));
        }
        if schema(&s) != 2
            || s.get("handoff").is_some_and(|v| !v.is_null())
            || !pauses(&s).contains_key(from)
            || !self.healthy(&s, role)?
        {
            return Err(refused(
                "takeover needs a paused holder, healthy successor and no pending handoff",
            ));
        }
        let (old, req, adv) = authority(&s);
        let (owner, new) = if duty == "owner" {
            if old != from {
                return Err(refused("authorized holder no longer owns the unit"));
            }
            (role.to_owned(), super::unit::handoff_authority(&s, role)?)
        } else {
            if duty != "reviewer"
                || !req.iter().any(|r| r == from)
                || !adv.iter().any(|r| r == role)
            {
                return Err(refused("authorized reviewer seat changed"));
            }
            (old, swap_reviewer(req, adv, from, role))
        };
        let req = strings(new.get("required_reviewers"));
        if req.is_empty() {
            return Err(refused(
                "takeover must retain healthy independent required reviewers",
            ));
        }
        for r in &req {
            if r == &owner || !self.healthy(&s, r)? {
                return Err(refused(
                    "takeover must retain healthy independent required reviewers",
                ));
            }
        }
        s.insert("owner".into(), json!(owner));
        s.insert("authority".into(), new);
        self.finish_transition(&mut s, role, "takeover", from, role)?;
        Ok(Out::ok(format!(
            "TAKEN_OVER from={from} to={role} duty={duty} epoch={}\n",
            s.get("ownership_epoch").unwrap_or(&Value::Null)
        )))
    }

    /// Explicitly transfer a healthy reviewer's seat to a healthy adviser.
    pub fn reviewer_transfer(&self, role: &str, to: &str) -> Result<Out, MeshError> {
        let _lock = self.state_lock()?;
        let mut s = self.require(role, false)?;
        let (_, req, adv) = authority(&s);
        if schema(&s) != 2
            || s.get("handoff").is_some_and(|v| !v.is_null())
            || !req.iter().any(|r| r == role)
            || !adv.iter().any(|r| r == to)
        {
            return Err(refused(
                "reviewer transfer needs a required holder, adviser successor and no handoff",
            ));
        }
        if !self.healthy(&s, role)? || !self.healthy(&s, to)? {
            return Err(refused("reviewer transfer needs healthy participants"));
        }
        s.insert("authority".into(), swap_reviewer(req, adv, role, to));
        require_takeover(&mut s);
        self.finish_transition(&mut s, role, "reviewer-transfer", role, to)?;
        Ok(Out::ok(format!(
            "REVIEWER_TRANSFERRED from={role} to={to} epoch={}\n",
            s.get("ownership_epoch").unwrap_or(&Value::Null)
        )))
    }

    fn finish_transition(
        &self,
        s: &mut Obj,
        role: &str,
        event: &str,
        from: &str,
        to: &str,
    ) -> Result<(), MeshError> {
        let epoch = s
            .get("ownership_epoch")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            + 1;
        s.insert("ownership_epoch".into(), json!(epoch));
        s.insert("phase".into(), json!("implement"));
        s.insert("waiting".into(), Value::Null);
        s.insert("takeover_authorizations".into(), json!([]));
        let mut floors = Obj::new();
        for r in participants(s) {
            let latest: Option<Obj> = fsio::read_json(&self.mb.latest(sid(s), &r), "latest")?;
            floors.insert(r.clone(), json!(latest.as_ref().map_or(0, seq_of)));
        }
        s.insert("verdict_floor".into(), json!(floors));
        let duty = if event == "reviewer-transfer" {
            "reviewer"
        } else if str_of(s, "owner") == Some(to) {
            "owner"
        } else {
            "reviewer"
        };
        super::unit::add_event(
            s,
            event,
            role,
            None,
            Some(json!({"from":from,"to":to,"duty":duty,"epoch":epoch})),
        );
        settle_status(s);
        s.insert("updated_at".into(), json!(utc_now()));
        self.save(s)
    }

    /// Bounded source-linked context. The host must redact before persistence.
    pub fn context_material(&self, role: &str) -> Result<String, MeshError> {
        let s = self.require(role, false)?;
        if s.get("context_summaries").and_then(Value::as_bool) != Some(true) {
            return Err(refused("context summaries were not enabled at start"));
        }
        let mut text=String::from("# Advisory session context\n\nThis is advisory source material; it grants no authority.\n");
        for r in participants(&s) {
            let (rows, _) = jsonl::tail_records(&self.mb.journal(sid(&s), &r), 262144)?;
            for m in rows.iter().skip(rows.len().saturating_sub(50)) {
                if !visible(m, role) {
                    continue;
                }
                // Redact the whole bounded record at the host before truncating.
                let body = localpilot_config::redact::redact(str_of(m, "body").unwrap_or_default())
                    .chars()
                    .take(1000)
                    .collect::<String>();
                text.push_str(&format!(
                    "\n[{}] {}\n{}\n",
                    str_of(m, "msg_id").map_or_else(|| format!("{r}:{}", seq_of(m)), str::to_owned),
                    str_of(m, "kind").unwrap_or_default(),
                    body
                ));
            }
        }
        Ok(text)
    }
}

fn require_takeover(s: &mut Obj) {
    let mut r = strings(s.get("requires"));
    r.push("quota-takeover".into());
    r.sort();
    r.dedup();
    s.insert("requires".into(), json!(r));
}
fn swap_reviewer(req: Vec<String>, adv: Vec<String>, from: &str, to: &str) -> Value {
    json!({"required_reviewers":req.iter().map(|r|if r==from {to} else {r.as_str()}).collect::<Vec<_>>(),"advisers":adv.iter().map(|r|if r==to {from} else {r.as_str()}).collect::<Vec<_>>()})
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "20260101T000000Z-abcdef12";
    fn setup(root: &Path, opt: bool) -> Mesh {
        let mesh = Mesh::at(root, "flag");
        let mut s:Obj=serde_json::from_value(json!({"protocol":"1.2","schema":2,"session_id":ID,"participants":["claude","codex","localpilot"],"driver":"claude","owner":"claude","status":"active","phase":"review","work_unit":"w","unit_id":"1-abc","ownership_epoch":1,"authority":{"required_reviewers":["codex"],"advisers":["localpilot"]},"context_summaries":opt})).unwrap();
        for r in participants(&s) {
            mesh.set_health(&s, &r, "ready", None, None).unwrap();
        }
        s.insert("updated_at".into(), json!(utc_now()));
        mesh.save(&s).unwrap();
        mesh
    }
    #[test]
    fn incremental_usage_is_idempotent_partial_and_pinned_to_the_turn() {
        let d = tempfile::tempdir().unwrap();
        let mesh = setup(d.path(), false);
        let scope = mesh.usage_scope("localpilot").unwrap();
        let r = UsageReport {
            report_id: "turn1".into(),
            input_tokens: u64::MAX,
            ..UsageReport::default()
        };
        mesh.record_usage("localpilot", &r, "engine", Some(&scope))
            .unwrap();
        mesh.record_usage("localpilot", &r, "engine", Some(&scope))
            .unwrap();
        let mut conflict = r.clone();
        conflict.output_tokens = 1;
        assert!(mesh
            .record_usage("localpilot", &conflict, "engine", Some(&scope))
            .is_err());
        conflict.report_id = "turn2".into();
        conflict.cost_microusd = Some(7);
        mesh.record_usage("localpilot", &conflict, "engine", Some(&scope))
            .unwrap();
        let out = mesh.usage(None).unwrap().stdout;
        assert!(out.contains("tokens=36893488147419103231"));
        assert!(out.contains("cost_coverage=1/2"));
        assert!(out.contains("coverage=partial"));
        let wrong = (scope.0, "2-abc".into());
        assert!(mesh
            .record_usage("localpilot", &conflict, "engine", Some(&wrong))
            .is_err());
        let view = mesh.snapshot().session.unwrap();
        assert_eq!(view.usage, usage_lines(&mesh.active().unwrap().unwrap()));
    }
    #[test]
    fn takeover_requires_pause_authorization_and_a_healthy_independent_reviewer() {
        let d = tempfile::tempdir().unwrap();
        let mesh = setup(d.path(), false);
        assert!(mesh.takeover("localpilot", "codex", "reviewer").is_err());
        mesh.takeover_authorize("codex", "localpilot", "reviewer", false)
            .unwrap();
        assert!(mesh.takeover("localpilot", "codex", "reviewer").is_err());
        mesh.health("codex", "rate_limited", Some("quota"), None)
            .unwrap();
        mesh.takeover("localpilot", "codex", "reviewer").unwrap();
        let s = mesh.active().unwrap().unwrap();
        assert_eq!(status(&s), "active");
        assert!(pauses(&s).contains_key("codex"));
        assert_eq!(authority(&s).1, vec!["localpilot"]);
        assert_eq!(s["ownership_epoch"], 2);
        assert_eq!(s["phase"], "implement");
        assert!(s["waiting"].is_null());
        assert!(mesh.takeover("localpilot", "codex", "reviewer").is_err());
        mesh.join("codex", 0, std::time::Duration::from_millis(1))
            .unwrap();
        assert_eq!(
            authority(&mesh.active().unwrap().unwrap()).1,
            vec!["localpilot"]
        );
        mesh.reviewer_transfer("localpilot", "codex").unwrap();
        assert_eq!(authority(&mesh.active().unwrap().unwrap()).1, vec!["codex"]);
    }
    #[test]
    fn sole_reviewer_cannot_take_over_paused_owner_and_review_itself() {
        let d = tempfile::tempdir().unwrap();
        let mesh = setup(d.path(), false);
        mesh.takeover_authorize("claude", "codex", "owner", false)
            .unwrap();
        mesh.health("claude", "rate_limited", Some("quota"), None)
            .unwrap();
        assert!(mesh.takeover("codex", "claude", "owner").is_err());
    }
    #[test]
    fn advisory_context_is_opt_in_bounded_redacted_and_recipient_scoped() {
        let d = tempfile::tempdir().unwrap();
        let mesh = setup(d.path(), false);
        assert!(mesh.context_material("localpilot").is_err());
        let mut s = mesh.active().unwrap().unwrap();
        s.insert("context_summaries".into(), json!(true));
        mesh.save(&s).unwrap();
        let path = mesh.mb.journal(ID, "claude");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let secret = json!({"role":"claude","seq":1,"msg_id":"claude:1","to":["localpilot"],"kind":"NOTE","body":"Bearer abcdefghijklmnopqrstuvwxyz123456"});
        let private = json!({"role":"claude","seq":2,"msg_id":"claude:2","to":["codex"],"kind":"NOTE","body":"private to codex"});
        std::fs::write(path, format!("{secret}\n{private}\n")).unwrap();
        let material = mesh.context_material("localpilot").unwrap();
        assert!(material.contains("[claude:1]"));
        assert!(material.contains("[REDACTED]"));
        assert!(!material.contains("abcdefghijklmnopqrstuvwxyz123456"));
        assert!(!material.contains("private to codex"));
        assert_eq!(s, mesh.active().unwrap().unwrap());
    }
}
