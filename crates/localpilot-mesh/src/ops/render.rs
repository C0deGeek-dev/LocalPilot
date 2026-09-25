//! The lines a participant prints that other tools and models read.

use serde_json::Value;

use super::{pause_blocks, str_of, strings, Obj};

/// `slug#ordinal`, or just the slug for a session that predates unit ids.
#[must_use]
pub fn unit_label(work_unit: Option<&str>, unit_id: Option<&str>) -> String {
    let Some(w) = work_unit.filter(|w| !w.is_empty()) else {
        return String::new();
    };
    match unit_id
        .and_then(|u| u.split('-').next())
        .filter(|o| !o.is_empty())
    {
        Some(o) => format!("{w}#{o}"),
        None => w.to_owned(),
    }
}

/// The directed-mail suffix of a heading: recipients, id, lineage.
pub(crate) fn route_marks(m: &Obj) -> String {
    let mut out = format!(
        " -> {} id={}",
        strings(m.get("to")).join(","),
        str_of(m, "msg_id").unwrap_or("None")
    );
    if let Some(r) = str_of(m, "reply_to") {
        out.push_str(&format!(" re={r}"));
    }
    if m.get("forward").and_then(Value::as_bool).unwrap_or(false) {
        out.push_str(" fwd");
    }
    if m.get("broadcast").and_then(Value::as_bool).unwrap_or(false) {
        out.push_str(" broadcast");
    }
    out
}

fn list_or_dash(v: &[String]) -> String {
    if v.is_empty() {
        "-".to_owned()
    } else {
        v.join(",")
    }
}

pub(crate) fn authority_line(
    owner: &str,
    required: &[String],
    advisers: &[String],
    label: Option<&str>,
) -> String {
    let unit = label
        .filter(|l| !l.is_empty())
        .map(|l| format!(" unit={l}"))
        .unwrap_or_default();
    format!(
        "AUTHORITY{unit} owner={owner} required={} advisers={}",
        list_or_dash(required),
        list_or_dash(advisers)
    )
}

pub(crate) fn pause_line(s: &Obj, role: &str, rec: &Obj) -> String {
    format!(
        "PAUSE {role} {} reason={} resume_at={}",
        if pause_blocks(s, role) {
            "blocking"
        } else {
            "informational"
        },
        str_of(rec, "reason")
            .filter(|x| !x.is_empty())
            .unwrap_or("-"),
        str_of(rec, "resume_at")
            .filter(|x| !x.is_empty())
            .unwrap_or("-")
    )
}

pub(crate) fn companion_line(c: &Obj) -> String {
    format!(
        "COMPANION {} root={} vcs={} write={} why={}",
        str_of(c, "name").unwrap_or_default(),
        str_of(c, "root").unwrap_or_default(),
        str_of(c, "vcs").filter(|x| !x.is_empty()).unwrap_or("git"),
        strings(c.get("write")).join(","),
        str_of(c, "why").unwrap_or_default()
    )
}

pub(crate) fn absent_line(c: &Obj) -> String {
    format!(
        "COMPANION_ABSENT name={} reason={} write={} why={}",
        str_of(c, "name").unwrap_or_default(),
        str_of(c, "reason").unwrap_or_default(),
        strings(c.get("write")).join(","),
        str_of(c, "why").unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unit_labels_drop_a_missing_ordinal() {
        assert_eq!(unit_label(Some("fix"), Some("2-abc")), "fix#2");
        assert_eq!(unit_label(Some("fix"), None), "fix");
        assert_eq!(unit_label(None, Some("2-abc")), "");
    }

    #[test]
    fn route_marks_name_recipients_and_lineage() {
        let m: Obj = serde_json::from_value(json!({
            "to": ["codex", "localpilot"], "msg_id": "claude:3", "reply_to": "codex:2",
            "forward": true, "broadcast": false
        }))
        .unwrap();
        assert_eq!(
            route_marks(&m),
            " -> codex,localpilot id=claude:3 re=codex:2 fwd"
        );
    }
}
