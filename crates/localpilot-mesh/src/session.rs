//! Resolving the active session (spec S-7, L-8) and checking the protocol a
//! record needs (spec V-3).

use serde_json::{Map, Value};

use crate::error::MeshError;
use crate::fsio;
use crate::layout::{Mailbox, SENTINEL_PREFIX, SESSION_V1, SESSION_V2};
use crate::records::{Pointer, Session};
use crate::{FEATURES, PROTOCOL};

/// Refuse a record that needs another major protocol version, or a feature
/// this build does not implement. A missing version is 1.0; a newer minor
/// version and unknown optional keys are accepted.
///
/// # Errors
/// [`MeshError::Unsupported`], naming `what` and the version or feature.
pub fn check_protocol(record: &Map<String, Value>, what: &str) -> Result<(), MeshError> {
    if let Some(v) = record.get("protocol") {
        let v = match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let major = |s: &str| s.split('.').next().unwrap_or_default().to_owned();
        if major(&v) != major(PROTOCOL) {
            return Err(MeshError::Unsupported(format!(
                "{what} uses protocol {v}; this build speaks {}.x",
                major(PROTOCOL)
            )));
        }
    }
    if let Some(Value::Array(required)) = record.get("requires") {
        let unknown: Vec<String> = required
            .iter()
            .map(|f| f.as_str().map_or_else(|| f.to_string(), str::to_owned))
            .filter(|f| !FEATURES.contains(&f.as_str()))
            .collect();
        if !unknown.is_empty() {
            return Err(MeshError::Unsupported(format!(
                "{what} requires {}, which this build does not support",
                unknown.join(", ")
            )));
        }
    }
    Ok(())
}

/// The pointer to the active session, or `None` when the slot is free.
///
/// # Errors
/// [`MeshError::Corrupt`] for an unparseable pointer, a sentinel and pointer
/// that disagree, or an active sentinel with no pointer (spec S-7).
pub fn pointer(mb: &Mailbox) -> Result<Option<Pointer>, MeshError> {
    let Some(raw) = fsio::read_bytes(&mb.active_v1())? else {
        return Ok(None);
    };
    let text = String::from_utf8_lossy(&raw);
    if let Some(rest) = text.strip_prefix(SENTINEL_PREFIX) {
        let sid = rest.split(':').next().unwrap_or_default().trim().to_owned();
        if let Some(p) = fsio::read_json::<Pointer>(&mb.active_v2(), "active.v2.json")? {
            if p.session_id == sid {
                return Ok(Some(Pointer {
                    schema: Some(2),
                    ..p
                }));
            }
            return Err(MeshError::Corrupt(format!(
                "active.json marks N-party session {sid} but active.v2.json names {}; refusing to guess which is live",
                p.session_id
            )));
        }
        // A crash between the two close steps leaves the sentinel alone. If
        // its session is finished or parked the slot is free; else corrupt.
        let record = if is_session_id(&sid) {
            fsio::read_json::<Session>(&mb.session_dir(&sid).join(SESSION_V2), "session.v2.json")?
        } else {
            None
        };
        if record.is_some_and(|s| s.is_terminal() || s.status == "parked") {
            return Ok(None);
        }
        return Err(MeshError::Corrupt(format!(
            "active.json marks an N-party session ({sid}) but active.v2.json is missing and that session is not closed or parked"
        )));
    }
    let p: Pointer = serde_json::from_slice(&raw)
        .map_err(|e| MeshError::Corrupt(format!("active.json is not valid JSON ({e})")))?;
    Ok(Some(Pointer {
        schema: Some(1),
        ..p
    }))
}

/// The active session record, checked against this build's protocol.
///
/// # Errors
/// [`MeshError::Corrupt`] for a pointer whose record is missing or ambiguous
/// (spec L-8), [`MeshError::Unsupported`] per [`check_protocol`].
pub fn active(mb: &Mailbox) -> Result<Option<Session>, MeshError> {
    let Some(p) = pointer(mb)? else {
        return Ok(None);
    };
    if !is_session_id(&p.session_id) {
        return Err(MeshError::Corrupt(format!(
            "the pointer names an invalid session id {:?}",
            p.session_id
        )));
    }
    let dir = mb.session_dir(&p.session_id);
    let (v1, v2) = (dir.join(SESSION_V1), dir.join(SESSION_V2));
    if v1.exists() && v2.exists() {
        return Err(MeshError::Corrupt(format!(
            "ambiguous session directory {:?}: holds both {SESSION_V1} and {SESSION_V2}",
            p.session_id
        )));
    }
    let file = if p.schema == Some(2) { v2 } else { v1 };
    let shown = mb.relative(&file);
    let Some(record) = fsio::read_json::<Map<String, Value>>(&file, &shown)? else {
        return Err(MeshError::Corrupt(format!(
            "the pointer names session {}, but its record is missing; refusing to guess",
            p.session_id
        )));
    };
    check_protocol(&record, &format!("session {}", p.session_id))?;
    let session: Session = serde_json::from_value(Value::Object(record))
        .map_err(|e| MeshError::Corrupt(format!("{shown} is not a session record ({e})")))?;
    Ok(Some(session))
}

/// Session ids are generated (`YYYYMMDDTHHMMSSZ-<hex>`); anything path-like is
/// refused before it becomes part of a path.
fn is_session_id(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    const SID: &str = "20260101T000000Z-abcdef12";

    fn write(path: &std::path::Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    fn schema1(dir: &std::path::Path, extra: Value) -> Mailbox {
        let mb = Mailbox::at(dir);
        write(
            &mb.active_v1(),
            &json!({"session_id": SID, "driver": "claude", "status": "active"}).to_string(),
        );
        let mut rec = json!({"session_id": SID, "status": "active", "work_unit": "w"});
        rec.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        write(&mb.session_dir(SID).join(SESSION_V1), &rec.to_string());
        mb
    }

    #[test]
    fn no_pointer_means_no_session() {
        let dir = tempfile::tempdir().unwrap();
        assert!(active(&Mailbox::at(dir.path())).unwrap().is_none());
    }

    #[test]
    fn a_schema_one_session_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let s = active(&schema1(dir.path(), json!({}))).unwrap().unwrap();
        assert_eq!(s.session_id, SID);
        assert_eq!(s.schema(), 1);
    }

    #[test]
    fn a_sentinel_and_its_pointer_resolve_to_schema_two() {
        let dir = tempfile::tempdir().unwrap();
        let mb = Mailbox::at(dir.path());
        write(
            &mb.active_v1(),
            &format!("{SENTINEL_PREFIX}{SID}: this mailbox needs a newer build"),
        );
        write(
            &mb.active_v2(),
            &json!({"session_id": SID, "status": "active", "schema": 2}).to_string(),
        );
        write(
            &mb.session_dir(SID).join(SESSION_V2),
            &json!({"session_id": SID, "status": "active", "schema": 2, "participants": ["claude", "codex", "localpilot"]}).to_string(),
        );
        let s = active(&mb).unwrap().unwrap();
        assert_eq!(s.schema(), 2);
        assert_eq!(s.participants().len(), 3);
    }

    #[test]
    fn a_sentinel_without_its_pointer_is_free_only_when_that_session_is_over() {
        let dir = tempfile::tempdir().unwrap();
        let mb = Mailbox::at(dir.path());
        write(&mb.active_v1(), &format!("{SENTINEL_PREFIX}{SID}: x"));
        let rec = mb.session_dir(SID).join(SESSION_V2);
        write(
            &rec,
            &json!({"session_id": SID, "status": "active", "schema": 2}).to_string(),
        );
        assert!(matches!(active(&mb), Err(MeshError::Corrupt(_))));
        write(
            &rec,
            &json!({"session_id": SID, "status": "completed", "schema": 2}).to_string(),
        );
        assert!(active(&mb).unwrap().is_none());
    }

    #[test]
    fn a_pointer_to_a_missing_record_is_corrupt_not_absent() {
        let dir = tempfile::tempdir().unwrap();
        let mb = schema1(dir.path(), json!({}));
        fs::remove_file(mb.session_dir(SID).join(SESSION_V1)).unwrap();
        let err = active(&mb).unwrap_err();
        assert!(
            matches!(err, MeshError::Corrupt(ref m) if m.contains("record is missing")),
            "{err}"
        );
    }

    #[test]
    fn a_garbage_record_is_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let mb = schema1(dir.path(), json!({}));
        fs::write(mb.session_dir(SID).join(SESSION_V1), "{not json").unwrap();
        assert!(matches!(active(&mb), Err(MeshError::Corrupt(_))));
        fs::write(mb.active_v1(), "{").unwrap();
        assert!(matches!(active(&mb), Err(MeshError::Corrupt(_))));
    }

    #[test]
    fn another_major_version_or_an_unknown_required_feature_is_refused() {
        for extra in [
            json!({"protocol": "2.0"}),
            json!({"requires": ["x-test.feature"]}),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let err = active(&schema1(dir.path(), extra.clone())).unwrap_err();
            assert!(matches!(err, MeshError::Unsupported(_)), "{extra}: {err}");
        }
        let dir = tempfile::tempdir().unwrap();
        assert!(
            active(&schema1(dir.path(), json!({"protocol": "1.9"})))
                .unwrap()
                .is_some(),
            "a newer minor is accepted"
        );
    }

    #[test]
    fn a_path_like_session_id_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mb = Mailbox::at(dir.path());
        write(
            &mb.active_v1(),
            &json!({"session_id": "../../etc", "status": "active"}).to_string(),
        );
        assert!(matches!(active(&mb), Err(MeshError::Corrupt(_))));
    }
}
