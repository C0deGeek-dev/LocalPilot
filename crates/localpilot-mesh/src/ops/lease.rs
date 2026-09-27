//! A write-authority lease: the session's answer to "may this participant
//! write the tree right now?", in the form the permission engine consults
//! before every tool effect.
//!
//! The lease is granted only while the role owns the tree (spec U-2), and
//! records the session, unit and ownership epoch it was granted in. It is
//! re-read from the mailbox on every [`Lease::current`] call, so a handoff, a
//! pause, a park, a new unit or a hand-away-and-back revokes it at the next
//! permission decision. It is authority only: *where* a write may land stays
//! the permission profile's own policy. A command already running is not
//! stopped.
//!
//! Library-only until the participant engine attaches it to its model turns.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use localpilot_sandbox::{Lease, LeaseState};

use super::read::num_at;
use super::unit::write_denied;
use super::{sid, str_of, Mesh, Obj};
use crate::error::MeshError;

/// What a lease was granted against.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Grant {
    session_id: String,
    unit_id: Option<String>,
    epoch: i64,
    /// For diagnostics only: the spec does not make the phase a write
    /// condition.
    phase: Option<String>,
}

/// The write authority of one role in one session.
#[derive(Debug)]
pub struct SessionLease {
    mesh: Mesh,
    role: String,
    grant: Grant,
    expires_at: SystemTime,
}

impl SessionLease {
    /// Grant `role` a lease valid for `ttl`, if it owns the tree now.
    ///
    /// # Errors
    /// [`MeshError::Refused`] when the role may not write now (not the owner,
    /// a pending handoff, a paused, parked or closed session, or no session),
    /// and any error reading the mailbox.
    pub fn grant(mesh: Mesh, role: &str, ttl: Duration) -> Result<Self, MeshError> {
        // Pin and check the same snapshot: a record read twice could pin one
        // unit and authorize against the next.
        let s = mesh.require(role, true)?;
        let grant = Grant::of(&s);
        authorize(&grant, role, &s).map_err(MeshError::Refused)?;
        Ok(Self {
            mesh,
            role: role.to_owned(),
            grant,
            expires_at: SystemTime::now() + ttl,
        })
    }

    /// A lease for `role` that is fail-closed by construction: the session's
    /// lease when the role owns the tree now, otherwise one that never lets
    /// it write. A participant that gains ownership later acquires again.
    #[must_use]
    pub fn acquire(mesh: Mesh, role: &str, ttl: Duration) -> Arc<dyn Lease> {
        match Self::grant(mesh, role, ttl) {
            Ok(lease) => Arc::new(lease),
            Err(e) => Arc::new(NoWrite(e.to_string())),
        }
    }

    /// The phase the lease was granted in, for diagnostics.
    #[must_use]
    pub fn phase(&self) -> Option<&str> {
        self.grant.phase.as_deref()
    }

    /// Every condition but expiry, decided on one fresh read of the session.
    fn check_authority(&self) -> Result<(), String> {
        let s = self
            .mesh
            .require(&self.role, true)
            .map_err(|e| e.to_string())?;
        authorize(&self.grant, &self.role, &s)
    }
}

impl Grant {
    fn of(s: &Obj) -> Self {
        Self {
            session_id: sid(s).to_owned(),
            unit_id: str_of(s, "unit_id").map(str::to_owned),
            epoch: num_at(s, "ownership_epoch"),
            phase: str_of(s, "phase").map(str::to_owned),
        }
    }
}

/// Whether one session record still honours `grant` for `role`: the same
/// session, unit and ownership epoch, and spec U-2 through the predicate
/// `guard-write` uses. Pure, so every check sees a single snapshot.
fn authorize(grant: &Grant, role: &str, s: &Obj) -> Result<(), String> {
    let now = Grant::of(s);
    if now.session_id != grant.session_id {
        return Err(format!("the active session is now {}", now.session_id));
    }
    if now.unit_id != grant.unit_id {
        return Err("the work unit changed".to_owned());
    }
    if now.epoch != grant.epoch {
        return Err("ownership changed hands".to_owned());
    }
    write_denied(s, role).map_or(Ok(()), Err)
}

impl Lease for SessionLease {
    fn current(&self) -> LeaseState {
        if SystemTime::now() >= self.expires_at {
            return LeaseState::Denied("the write lease expired".to_owned());
        }
        match self.check_authority() {
            Ok(()) => LeaseState::Owner,
            Err(reason) => LeaseState::Denied(reason),
        }
    }
}

/// The lease of a participant that does not own the tree.
#[derive(Debug)]
struct NoWrite(String);

impl Lease for NoWrite {
    fn current(&self) -> LeaseState {
        LeaseState::Denied(self.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::path::Path;

    const SID: &str = "20260101T000000Z-abcdef12";

    /// A schema-1 pair session under `dir`, owned by `owner`.
    fn session(dir: &Path, owner: &str) {
        let sd = dir.join(".pair-programming").join("sessions").join(SID);
        std::fs::create_dir_all(sd.join("journal")).unwrap();
        write_session(
            dir,
            &json!({"session_id": SID, "driver": "claude", "navigator": "localpilot", "owner": owner,
                    "participants": ["claude", "localpilot"],
                    "status": "active", "phase": "review", "ownership_epoch": 0, "work_unit": "w",
                    "unit_id": "1-abc", "task": "t", "handoff": null, "protocol": "1.0"}),
        );
        std::fs::write(
            dir.join(".pair-programming").join("active.json"),
            json!({"session_id": SID, "driver": "claude", "status": "active", "updated_at": "2026-01-01T00:00:00Z"})
                .to_string(),
        )
        .unwrap();
    }

    fn session_path(dir: &Path) -> std::path::PathBuf {
        dir.join(".pair-programming")
            .join("sessions")
            .join(SID)
            .join("session.json")
    }

    fn write_session(dir: &Path, rec: &Value) {
        std::fs::write(session_path(dir), rec.to_string()).unwrap();
    }

    fn edit(dir: &Path, key: &str, value: Value) {
        let mut s: serde_json::Map<String, Value> =
            serde_json::from_str(&std::fs::read_to_string(session_path(dir)).unwrap()).unwrap();
        s.insert(key.into(), value);
        write_session(dir, &Value::Object(s));
    }

    fn lease(dir: &Path, role: &str) -> Result<SessionLease, MeshError> {
        SessionLease::grant(Mesh::at(dir, "flag"), role, Duration::from_secs(3600))
    }

    fn denied(lease: &SessionLease) -> bool {
        matches!(lease.current(), LeaseState::Denied(_))
    }

    #[test]
    fn only_the_owner_of_an_active_session_is_granted_a_lease() {
        let dir = tempfile::tempdir().unwrap();
        session(dir.path(), "claude");
        assert!(lease(dir.path(), "localpilot").is_err(), "a navigator");
        let owner = lease(dir.path(), "claude").unwrap();
        assert_eq!(owner.current(), LeaseState::Owner);
        assert_eq!(owner.phase(), Some("review"));

        edit(
            dir.path(),
            "handoff",
            json!({"epoch": 1, "from": "claude", "to": "localpilot"}),
        );
        assert!(lease(dir.path(), "claude").is_err(), "a pending handoff");
        edit(dir.path(), "handoff", Value::Null);
        edit(dir.path(), "status", json!("paused"));
        assert!(lease(dir.path(), "claude").is_err(), "a paused session");
        edit(dir.path(), "status", json!("parked"));
        assert!(lease(dir.path(), "claude").is_err(), "a parked session");

        let empty = tempfile::tempdir().unwrap();
        assert!(lease(empty.path(), "claude").is_err(), "no session");
    }

    #[test]
    fn every_change_of_authority_revokes_the_lease_at_the_next_check() {
        // Bug it prevents: a participant that was the owner once keeping
        // write access after the session moved on.
        let cases: [(&str, Value); 7] = [
            (
                "handoff",
                json!({"epoch": 1, "from": "claude", "to": "localpilot"}),
            ),
            ("owner", json!("localpilot")),
            ("unit_id", json!("2-def")),
            ("ownership_epoch", json!(2)),
            ("status", json!("paused")),
            ("status", json!("parked")),
            ("status", json!("completed")),
        ];
        for (key, value) in cases {
            let dir = tempfile::tempdir().unwrap();
            session(dir.path(), "claude");
            let l = lease(dir.path(), "claude").unwrap();
            assert_eq!(l.current(), LeaseState::Owner);
            edit(dir.path(), key, value.clone());
            assert!(denied(&l), "{key}={value}");
        }
    }

    #[test]
    fn handing_away_and_back_does_not_revive_an_old_lease() {
        let dir = tempfile::tempdir().unwrap();
        session(dir.path(), "claude");
        let l = lease(dir.path(), "claude").unwrap();
        // Away and back again: the owner is claude once more, but the epoch
        // moved twice.
        edit(dir.path(), "ownership_epoch", json!(2));
        assert!(denied(&l));
        assert_eq!(
            lease(dir.path(), "claude").unwrap().current(),
            LeaseState::Owner
        );
    }

    #[test]
    fn an_unreadable_or_missing_session_denies_and_never_panics() {
        let dir = tempfile::tempdir().unwrap();
        session(dir.path(), "claude");
        let l = lease(dir.path(), "claude").unwrap();
        std::fs::write(session_path(dir.path()), "{ not json").unwrap();
        assert!(denied(&l), "corrupt session record");

        session(dir.path(), "claude");
        assert_eq!(l.current(), LeaseState::Owner);
        std::fs::remove_file(session_path(dir.path())).unwrap();
        assert!(denied(&l), "missing session record");

        session(dir.path(), "claude");
        std::fs::remove_file(dir.path().join(".pair-programming").join("active.json")).unwrap();
        assert!(denied(&l), "no active pointer");

        session(dir.path(), "claude");
        edit(dir.path(), "protocol", json!("9.0"));
        assert!(denied(&l), "a protocol this build refuses");
    }

    #[test]
    fn acquiring_as_a_non_owner_yields_a_lease_that_never_writes() {
        let dir = tempfile::tempdir().unwrap();
        session(dir.path(), "claude");
        let navigator = SessionLease::acquire(
            Mesh::at(dir.path(), "flag"),
            "localpilot",
            Duration::from_secs(60),
        );
        assert!(
            matches!(navigator.current(), LeaseState::Denied(ref r) if r.contains("WRITE_DENIED"))
        );
        // Even once it owns the tree: it must acquire again.
        edit(dir.path(), "owner", json!("localpilot"));
        assert!(matches!(navigator.current(), LeaseState::Denied(_)));
        let owner = SessionLease::acquire(
            Mesh::at(dir.path(), "flag"),
            "localpilot",
            Duration::from_secs(60),
        );
        assert_eq!(owner.current(), LeaseState::Owner);
    }

    fn record(owner: &str, unit: &str, epoch: i64) -> Obj {
        json!({"session_id": SID, "owner": owner, "status": "active", "handoff": null,
               "unit_id": unit, "ownership_epoch": epoch, "phase": "review"})
        .as_object()
        .unwrap()
        .clone()
    }

    #[test]
    fn a_stale_grant_is_never_authorized_by_a_newer_record() {
        // Bug it prevents: pinning the unit from one read and checking the
        // owner on another, so a lease from unit 1 passes in unit 2 because
        // its role still owns the tree there.
        let grant = Grant::of(&record("claude", "1-abc", 0));
        assert_eq!(
            authorize(&grant, "claude", &record("claude", "1-abc", 0)),
            Ok(())
        );
        assert_eq!(
            authorize(&grant, "claude", &record("claude", "2-def", 0)),
            Err("the work unit changed".to_owned())
        );
        assert_eq!(
            authorize(&grant, "claude", &record("claude", "1-abc", 2)),
            Err("ownership changed hands".to_owned())
        );
        // U-2 comes from the same record as the pins, through guard-write's
        // own predicate.
        let mut offered = record("claude", "1-abc", 0);
        offered.insert("handoff".into(), json!({"to": "localpilot"}));
        assert_eq!(
            authorize(&grant, "claude", &offered),
            Err("WRITE_DENIED status=active owner=claude handoff=yes".to_owned())
        );
        assert_eq!(
            write_denied(&offered, "claude"),
            authorize(&grant, "claude", &offered).err()
        );
    }

    #[test]
    fn an_expired_lease_is_denied() {
        let dir = tempfile::tempdir().unwrap();
        session(dir.path(), "claude");
        let l =
            SessionLease::grant(Mesh::at(dir.path(), "flag"), "claude", Duration::ZERO).unwrap();
        assert_eq!(
            l.current(),
            LeaseState::Denied("the write lease expired".to_owned())
        );
    }
}
