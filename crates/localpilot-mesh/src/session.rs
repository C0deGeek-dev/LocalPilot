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
    let required = match record.get("requires") {
        None | Some(Value::Null) => None,
        Some(Value::Array(list)) => Some(list),
        // A malformed claim is not "no requirement": fail closed.
        Some(other) => {
            return Err(MeshError::Corrupt(format!(
                "{what} has a malformed `requires` ({other}); it must be a list of feature names"
            )))
        }
    };
    if let Some(required) = required {
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

/// The sign-off policies a session record may name (spec M-6a).
pub const SIGNOFFS: &[&str] = &["strict", "legacy"];

/// [`check_protocol`] for a session record, then its sign-off claim (spec
/// M-6a). `envelope` is the schema of the file the record was read through.
/// A strict session records `signoff: "strict"`, requires `strict-signoff`
/// and is schema 2; a record that claims strict sign-off any other way, or
/// names a policy this build does not know, is refused and never read as
/// legacy, since a reader that skipped the claim would close on a prefix AGREE.
///
/// # Errors
/// As [`check_protocol`]; [`MeshError::Unsupported`] for an unknown or
/// inconsistent sign-off claim.
pub fn check_session(
    record: &Map<String, Value>,
    what: &str,
    envelope: u32,
) -> Result<(), MeshError> {
    check_protocol(record, what)?;
    let policy = record.get("signoff");
    let strict = policy.and_then(Value::as_str) == Some("strict");
    if let Some(p) = policy {
        if !p.as_str().is_some_and(|p| SIGNOFFS.contains(&p)) {
            return Err(MeshError::Unsupported(format!(
                "{what} records sign-off policy {p}, which this build does not know; nothing was changed"
            )));
        }
    }
    let required = record
        .get("requires")
        .and_then(Value::as_array)
        .is_some_and(|l| l.iter().any(|f| f.as_str() == Some("strict-signoff")));
    let schema = record.get("schema").and_then(Value::as_u64).unwrap_or(1);
    check_participants(record, what, schema)?;
    if (strict || required) && !(strict && required && envelope == 2 && schema == 2) {
        return Err(MeshError::Unsupported(format!(
            "{what} carries an inconsistent strict sign-off claim (signoff={}, requires strict-signoff={}, schema {schema}, read as schema {envelope}); \
             a strict session records signoff=strict, requires strict-signoff and is schema 2. It is refused; nothing was changed",
            policy.and_then(Value::as_str).unwrap_or("None"),
            if required { "yes" } else { "no" },
        )));
    }
    Ok(())
}

/// The hosts a participant name starts with (spec §1).
pub const HOSTS: &[&str] = &["claude", "codex", "localpilot"];

/// Whether `name` is a participant name: a host, optionally followed by `-`
/// and 1-32 lowercase ASCII letters or digits naming an instance (spec §1).
/// Participant names become file names, so nothing else is accepted.
#[must_use]
pub fn is_participant_name(name: &str) -> bool {
    HOSTS.iter().any(|h| {
        name == *h
            || name
                .strip_prefix(h)
                .and_then(|rest| rest.strip_prefix('-'))
                .is_some_and(|inst| {
                    (1..=32).contains(&inst.len())
                        && inst
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
                })
    })
}

/// Refuse a record whose participants are not participant names, or that
/// names an instance without schema 2 and the `participant-instances`
/// feature (spec §1, V-3).
fn check_participants(
    record: &Map<String, Value>,
    what: &str,
    schema: u64,
) -> Result<(), MeshError> {
    let Some(list) = record.get("participants").and_then(Value::as_array) else {
        return Ok(());
    };
    let mut instance = false;
    for p in list {
        let Some(name) = p.as_str().filter(|n| is_participant_name(n)) else {
            return Err(MeshError::Corrupt(format!(
                "{what} names participant {p}, which is not a participant name"
            )));
        };
        instance |= !HOSTS.contains(&name);
    }
    let declared = record
        .get("requires")
        .and_then(Value::as_array)
        .is_some_and(|l| {
            l.iter()
                .any(|f| f.as_str() == Some("participant-instances"))
        });
    if instance && !(declared && schema == 2) {
        return Err(MeshError::Unsupported(format!(
            "{what} names a participant instance but is not a schema-2 session requiring participant-instances; nothing was changed"
        )));
    }
    Ok(())
}

/// Whether a session closes on strict sign-off (M-6a) rather than the legacy
/// prefix rule (M-6). Any claim reads as strict; [`check_session`] has
/// already refused a claim the record cannot protect.
#[must_use]
pub fn is_strict(record: &Map<String, Value>) -> bool {
    record.get("signoff").and_then(Value::as_str) == Some("strict")
        || record
            .get("requires")
            .and_then(Value::as_array)
            .is_some_and(|l| l.iter().any(|f| f.as_str() == Some("strict-signoff")))
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
    // Mailbox content is UTF-8 (spec L-4); a pointer that is not is corrupt,
    // never repaired into something that might match.
    let text = std::str::from_utf8(&raw)
        .map_err(|e| MeshError::Corrupt(format!("active.json is not valid UTF-8 ({e})")))?;
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
    let Some(record) = active_record(mb)? else {
        return Ok(None);
    };
    serde_json::from_value(Value::Object(record))
        .map(Some)
        .map_err(|e| MeshError::Corrupt(format!("the session record is not a session ({e})")))
}

/// The active session record as read, after the same checks as [`active`]:
/// for operations that must rewrite it with every key it holds.
///
/// # Errors
/// As [`active`].
pub fn active_record(mb: &Mailbox) -> Result<Option<Map<String, Value>>, MeshError> {
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
    check_session(
        &record,
        &format!("session {}", p.session_id),
        if p.schema == Some(2) { 2 } else { 1 },
    )?;
    let session: Session = serde_json::from_value(Value::Object(record.clone()))
        .map_err(|e| MeshError::Corrupt(format!("{shown} is not a session record ({e})")))?;
    // The record must be the schema its file and pointer say it is: a
    // `session.v2.json` without `schema: 2` would otherwise read as schema 1
    // and slip past the participant check below, and a `session.json`
    // claiming schema 2 is not one an older build can operate.
    let expected = if p.schema == Some(2) { 2 } else { 1 };
    if session.schema.unwrap_or(1) != expected {
        return Err(MeshError::Corrupt(format!(
            "{shown} records schema {} but is a schema-{expected} session file",
            session
                .schema
                .map_or_else(|| "none".to_owned(), |s| s.to_string())
        )));
    }
    // Schema 2 names its participants; only schema 1 is implicitly the
    // historic pair. Guessing would give the session the wrong identities.
    if session.schema() == 2 && session.participants.as_ref().is_none_or(Vec::is_empty) {
        return Err(MeshError::Corrupt(format!(
            "{shown} is a schema-2 session with no participants"
        )));
    }
    Ok(Some(record))
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
    fn a_malformed_requires_is_refused_not_ignored() {
        let dir = tempfile::tempdir().unwrap();
        for bad in [
            json!("x-test.feature"),
            json!({"x-test.feature": true}),
            json!(7),
        ] {
            let p = dir.path().join("codex.jsonl");
            fs::write(
                &p,
                format!(
                    "{}
",
                    json!({"seq": 1, "requires": bad})
                ),
            )
            .unwrap();
            assert!(
                matches!(crate::jsonl::records(&p), Err(MeshError::Corrupt(_))),
                "{bad}"
            );
        }
        let p = dir.path().join("ok.jsonl");
        fs::write(
            &p,
            "{\"seq\":1,\"requires\":[]}
",
        )
        .unwrap();
        assert_eq!(
            crate::jsonl::records(&p).unwrap().len(),
            1,
            "an empty list requires nothing"
        );
    }

    #[test]
    fn a_sentinel_that_is_not_utf8_is_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let mb = Mailbox::at(dir.path());
        let mut raw = format!("{SENTINEL_PREFIX}{SID}: x").into_bytes();
        raw.push(0xFF);
        fs::create_dir_all(mb.base()).unwrap();
        fs::write(mb.active_v1(), raw).unwrap();
        write(
            &mb.active_v2(),
            &json!({"session_id": SID, "status": "active", "schema": 2}).to_string(),
        );
        assert!(matches!(active(&mb), Err(MeshError::Corrupt(ref m)) if m.contains("UTF-8")));
    }

    #[test]
    fn a_schema_two_session_without_participants_is_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let mb = Mailbox::at(dir.path());
        write(&mb.active_v1(), &format!("{SENTINEL_PREFIX}{SID}: x"));
        write(
            &mb.active_v2(),
            &json!({"session_id": SID, "status": "active", "schema": 2}).to_string(),
        );
        let rec = mb.session_dir(SID).join(SESSION_V2);
        for body in [
            json!({"session_id": SID, "status": "active", "schema": 2}),
            json!({"session_id": SID, "status": "active", "schema": 2, "participants": []}),
        ] {
            write(&rec, &body.to_string());
            assert!(
                matches!(active(&mb), Err(MeshError::Corrupt(ref m)) if m.contains("no participants")),
                "{body}"
            );
        }
    }

    #[test]
    fn a_session_record_must_be_the_schema_its_file_says() {
        let dir = tempfile::tempdir().unwrap();
        let mb = Mailbox::at(dir.path());
        write(&mb.active_v1(), &format!("{SENTINEL_PREFIX}{SID}: x"));
        write(
            &mb.active_v2(),
            &json!({"session_id": SID, "status": "active", "schema": 2}).to_string(),
        );
        let v2 = mb.session_dir(SID).join(SESSION_V2);
        for body in [
            json!({"session_id": SID, "status": "active"}),
            json!({"session_id": SID, "status": "active", "participants": ["claude", "localpilot"]}),
            json!({"session_id": SID, "status": "active", "schema": 1, "participants": ["claude", "codex"]}),
        ] {
            write(&v2, &body.to_string());
            assert!(
                matches!(active(&mb), Err(MeshError::Corrupt(ref m)) if m.contains("is a schema-2 session file")),
                "{body}"
            );
        }
        // A schema-1 file claiming schema 2.
        let dir = tempfile::tempdir().unwrap();
        let mb = schema1(
            dir.path(),
            json!({"schema": 2, "participants": ["claude", "codex"]}),
        );
        assert!(
            matches!(active(&mb), Err(MeshError::Corrupt(ref m)) if m.contains("is a schema-1 session file"))
        );
    }

    #[test]
    fn participant_names_are_hosts_or_named_instances() {
        for ok in [
            "claude",
            "codex",
            "localpilot",
            "codex-sol",
            "claude-a1",
            &format!("codex-{}", "a".repeat(32)),
        ] {
            assert!(is_participant_name(ok), "{ok}");
        }
        for bad in [
            "",
            "copilot",
            "codex-",
            "codex-Sol",
            "codex-a.b",
            "codex_sol",
            "../codex",
            "codexsol",
            &format!("codex-{}", "a".repeat(33)),
            "codex-\u{212a}",
        ] {
            assert!(!is_participant_name(bad), "{bad}");
        }
    }

    #[test]
    fn a_strict_claim_is_honoured_only_in_its_one_shape() {
        let rec = |v: Value| v.as_object().cloned().unwrap_or_default();
        let good = rec(
            json!({"schema": 2, "signoff": "strict", "requires": ["strict-signoff"], "participants": ["claude", "codex"]}),
        );
        assert!(check_session(&good, "s", 2).is_ok());
        assert!(is_strict(&good));
        assert!(check_session(&rec(json!({"signoff": "legacy"})), "s", 1).is_ok());
        assert!(check_session(&rec(json!({})), "s", 1).is_ok());
        assert!(!is_strict(&rec(json!({"signoff": "legacy"}))));
        for (bad, env) in [
            (
                json!({"schema": 2, "signoff": "strict", "participants": ["claude", "codex"]}),
                2,
            ),
            (
                json!({"schema": 2, "signoff": "legacy", "requires": ["strict-signoff"], "participants": ["claude", "codex"]}),
                2,
            ),
            (
                json!({"schema": 2, "requires": ["strict-signoff"], "participants": ["claude", "codex"]}),
                2,
            ),
            (
                json!({"signoff": "strict", "requires": ["strict-signoff"]}),
                1,
            ),
            (
                json!({"schema": 2, "signoff": "strict", "requires": ["strict-signoff"], "participants": ["claude", "codex"]}),
                1,
            ),
            (json!({"signoff": "lenient"}), 1),
            (json!({"signoff": 1}), 1),
        ] {
            assert!(
                matches!(
                    check_session(&rec(bad.clone()), "s", env),
                    Err(MeshError::Unsupported(_))
                ),
                "{bad} read as schema {env}"
            );
        }
    }

    #[test]
    fn an_instance_needs_schema_two_and_its_feature() {
        let rec = |v: Value| v.as_object().cloned().unwrap_or_default();
        assert!(check_session(&rec(json!({"schema": 2, "requires": ["participant-instances"], "participants": ["codex-sol", "codex"]})), "s", 2).is_ok());
        assert!(matches!(
            check_session(
                &rec(json!({"schema": 2, "participants": ["codex-sol", "codex"]})),
                "s",
                2
            ),
            Err(MeshError::Unsupported(_))
        ));
        assert!(matches!(
            check_session(
                &rec(
                    json!({"schema": 2, "requires": ["participant-instances"], "participants": ["../x", "codex"]})
                ),
                "s",
                2
            ),
            Err(MeshError::Corrupt(_))
        ));
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
