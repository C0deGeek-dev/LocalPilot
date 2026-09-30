//! `snapshot`: the active session as typed data, for an observer such as the
//! cockpit. It reads and never writes, and it holds no role.
//!
//! A snapshot is assembled from several files, so a change landing between
//! two reads could mix two states. After assembling, the whole session
//! record and every participant's health are read again and compared with
//! what the snapshot used, value for value (timestamps have one-second
//! precision, so no single field can stand in for the rest). If anything
//! differs, the snapshot is taken again, at most [`ATTEMPTS`] times, and
//! otherwise it is marked inconsistent with what differed. A part that cannot
//! be read is named in `read_errors` beside the parts that could be read.
//!
//! Each journal is read only in its last [`WINDOW`] bytes, so a refresh costs
//! the same however long the session has run. What lies before the window is
//! said to be there (`recent_truncated`, `review_state`), never guessed at.

use serde::Serialize;
use serde_json::Value;

use super::{
    authority, delivery, participants, pauses, schema, sid, str_of, waiting_map, Mesh, Obj,
};
use crate::jsonl;

/// How many records per role a snapshot carries, newest last.
pub const RECENT: usize = 50;
/// How much of each journal a snapshot reads, from its end.
pub const WINDOW: u64 = 256 * 1024;
/// How many times a snapshot is taken before it is shown as inconsistent.
pub const ATTEMPTS: usize = 3;

/// What an observer sees of the mailbox.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Snapshot {
    /// The active (or paused, or parked) session; `None` when there is none.
    pub session: Option<SessionView>,
    /// Whether the session record was the same before and after the reads.
    pub consistent: bool,
    /// The identifying fields that moved while the snapshot was taken.
    pub inconsistent: Vec<String>,
    /// The parts that could not be read, with why.
    pub read_errors: Vec<String>,
}

/// The session record's state, as the snapshot saw it.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SessionView {
    pub session_id: String,
    pub schema: i64,
    pub status: String,
    pub phase: String,
    pub driver: String,
    pub owner: String,
    pub required_reviewers: Vec<String>,
    pub advisers: Vec<String>,
    pub unit_id: Option<String>,
    pub work_unit: Option<String>,
    pub ownership_epoch: i64,
    pub updated_at: Option<String>,
    pub delivery: String,
    /// The pending handoff, as the record holds it.
    pub handoff: Option<Value>,
    pub pauses: Value,
    pub waiting: Value,
    /// Lifecycle events the record carries (empty before protocol 1.1).
    pub events: Vec<Value>,
    /// Incremental usage, grouped by role, unit and source; always partial.
    pub usage: Vec<String>,
    pub context_summaries: bool,
    pub participants: Vec<ParticipantView>,
    /// The owner's latest review request in this unit, and its answers.
    pub review: Option<ReviewView>,
    /// `open` (a request in this unit is shown), `none` (the owner's whole
    /// journal was read and holds none), or `beyond_window` (none in the
    /// part read, and older records were not read).
    pub review_state: String,
}

/// One participant, as the snapshot saw it.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ParticipantView {
    pub role: String,
    pub health: String,
    pub health_updated_at: Option<String>,
    pub resume_at: Option<String>,
    /// `(sender, first, last)`: mail presented to this role but not
    /// acknowledged (acknowledged delivery only).
    pub unacked: Vec<(String, i64, i64)>,
    /// This role's last [`RECENT`] journal records, oldest first.
    pub recent: Vec<Value>,
    /// Whether this role's journal has older records than were read.
    pub recent_truncated: bool,
}

/// An open review request and the verdicts that answer it.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ReviewView {
    pub request: Value,
    /// Verdicts that name this request in `reply_to`: its answers.
    pub verdicts: Vec<Value>,
    /// Verdicts in this unit that name no request (a two-party session posts
    /// them without `reply_to`). Their link to this request is unknown, so
    /// they are never shown as its answers.
    pub unlinked: Vec<Value>,
}

/// The keys whose values differ between two readings of the session
/// record, in key order: the whole record is compared, not a few fields.
fn moved(before: &Obj, after: &Obj) -> Vec<String> {
    let mut keys: Vec<&String> = before.keys().chain(after.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter(|k| before.get(*k) != after.get(*k))
        .cloned()
        .collect()
}

impl Mesh {
    /// The active session as typed data; see the module docs for how it is
    /// kept from mixing two states.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot_reading(|m| m.active())
    }

    /// [`Mesh::snapshot`], reading the session record through `record`.
    fn snapshot_reading(
        &self,
        mut record: impl FnMut(&Self) -> Result<Option<Obj>, crate::MeshError>,
    ) -> Snapshot {
        let mut last = Snapshot::default();
        for _ in 0..ATTEMPTS {
            let before = match record(self) {
                Ok(s) => s,
                Err(e) => {
                    return Snapshot {
                        read_errors: vec![format!("session record: {e}")],
                        ..Snapshot::default()
                    }
                }
            };
            let Some(before) = before else {
                return Snapshot {
                    consistent: true,
                    ..Snapshot::default()
                };
            };
            let mut snap = Snapshot::default();
            let (view, health) = self.view(&before, &mut snap.read_errors);
            snap.session = Some(view);
            let after = match record(self) {
                Ok(Some(s)) => s,
                Ok(None) => Obj::new(),
                Err(e) => {
                    snap.read_errors.push(format!("session record: {e}"));
                    Obj::new()
                }
            };
            snap.inconsistent = moved(&before, &after);
            // Health lives in its own files: it too must be what was shown.
            for (r, used) in &health {
                if self.health_of(&before, r).ok().as_ref() != Some(used) {
                    snap.inconsistent.push(format!("health:{r}"));
                }
            }
            snap.consistent = snap.inconsistent.is_empty();
            if snap.consistent {
                return snap;
            }
            last = snap;
        }
        last
    }

    /// The session's view, and the health records it used, by role.
    fn view(&self, s: &Obj, errors: &mut Vec<String>) -> (SessionView, Vec<(String, Obj)>) {
        let (owner, required, advisers) = authority(s);
        let parts = participants(s);
        let mut journals: Vec<(String, Vec<Obj>, bool)> = Vec::new();
        let mut people = Vec::new();
        let mut health = Vec::new();
        for r in &parts {
            let mut p = ParticipantView {
                role: r.clone(),
                ..ParticipantView::default()
            };
            match self.health_of(s, r) {
                Ok(h) => {
                    health.push((r.clone(), h.clone()));
                    p.health = str_of(&h, "status").unwrap_or("unknown").to_owned();
                    p.health_updated_at = str_of(&h, "updated_at").map(str::to_owned);
                    p.resume_at = str_of(&h, "resume_at")
                        .filter(|x| !x.is_empty())
                        .map(str::to_owned);
                }
                Err(e) => errors.push(format!("{r} health: {e}")),
            }
            let unacked = if schema(s) == 1 {
                self.unacked_range(s, r).map(|u| {
                    u.map(|(a, b)| {
                        let peer = super::the_peer(s, r).unwrap_or_default();
                        vec![(peer, a, b)]
                    })
                    .unwrap_or_default()
                })
            } else {
                self.unacked_v2(s, r)
            };
            match unacked {
                Ok(u) => p.unacked = u,
                Err(e) => errors.push(format!("{r} cursor: {e}")),
            }
            match jsonl::tail_records(&self.mb.journal(sid(s), r), WINDOW) {
                Ok((rows, truncated)) => {
                    p.recent = rows
                        .iter()
                        .skip(rows.len().saturating_sub(RECENT))
                        .map(|m| Value::Object(m.clone()))
                        .collect();
                    p.recent_truncated = truncated;
                    journals.push((r.clone(), rows, truncated));
                }
                Err(e) => errors.push(format!("{r} journal: {e}")),
            }
            people.push(p);
        }
        let review = open_review(s, &journals);
        let view = SessionView {
            session_id: sid(s).to_owned(),
            schema: schema(s),
            status: str_of(s, "status").unwrap_or_default().to_owned(),
            phase: str_of(s, "phase").unwrap_or_default().to_owned(),
            driver: str_of(s, "driver").unwrap_or_default().to_owned(),
            owner,
            required_reviewers: required,
            advisers,
            unit_id: str_of(s, "unit_id").map(str::to_owned),
            work_unit: str_of(s, "work_unit").map(str::to_owned),
            ownership_epoch: s
                .get("ownership_epoch")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            updated_at: str_of(s, "updated_at").map(str::to_owned),
            delivery: delivery(s).to_owned(),
            handoff: s.get("handoff").filter(|h| !h.is_null()).cloned(),
            pauses: Value::Object(pauses(s)),
            waiting: Value::Object(waiting_map(s)),
            events: s
                .get("events")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            review: review.0,
            review_state: review.1.to_owned(),
            participants: people,
            usage: super::continuity::usage_lines(s),
            context_summaries: s
                .get("context_summaries")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        };
        (view, health)
    }
}

/// The owner's latest review request in the current unit, the verdicts that
/// name it, and the unit's verdicts that name no request; with the review
/// state (see [`SessionView::review_state`]). Only the read windows are
/// searched.
fn open_review(
    s: &Obj,
    journals: &[(String, Vec<Obj>, bool)],
) -> (Option<ReviewView>, &'static str) {
    let Some(owner) = str_of(s, "owner") else {
        return (None, "none");
    };
    let unit = s.get("unit_id");
    let Some((_, rows, owner_truncated)) = journals.iter().find(|(r, _, _)| r == owner) else {
        return (None, "none");
    };
    let Some(request) = rows
        .iter()
        .filter(|m| str_of(m, "kind") == Some("REVIEW_REQUEST") && m.get("unit_id") == unit)
        .last()
    else {
        return (
            None,
            if *owner_truncated {
                "beyond_window"
            } else {
                "none"
            },
        );
    };
    let mid = str_of(request, "msg_id");
    let mut verdicts = Vec::new();
    let mut unlinked = Vec::new();
    for m in journals
        .iter()
        .filter(|(r, _, _)| r != owner)
        .flat_map(|(_, rows, _)| rows.iter())
        .filter(|m| str_of(m, "kind") == Some("VERDICT") && m.get("unit_id") == unit)
    {
        match (mid, str_of(m, "reply_to")) {
            (Some(id), Some(to)) if id == to => verdicts.push(Value::Object(m.clone())),
            (_, Some(_)) => {}
            (_, None) => unlinked.push(Value::Object(m.clone())),
        }
    }
    (
        Some(ReviewView {
            request: Value::Object(request.clone()),
            verdicts,
            unlinked,
        }),
        "open",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(unit: &str, epoch: i64, at: &str) -> Obj {
        json!({"session_id": "s1", "schema": 2, "status": "active", "phase": "review",
               "owner": "claude", "participants": ["claude", "codex"], "unit_id": unit,
               "ownership_epoch": epoch, "updated_at": at, "pauses": {}})
        .as_object()
        .unwrap()
        .clone()
    }

    #[test]
    fn a_record_that_moves_between_readings_is_named() {
        let a = record("1-a", 0, "t1");
        assert!(moved(&a, &a.clone()).is_empty());
        assert_eq!(moved(&a, &record("2-b", 0, "t1")), ["unit_id"]);
        assert_eq!(
            moved(&a, &record("1-a", 1, "t2")),
            ["ownership_epoch", "updated_at"]
        );
        assert_eq!(moved(&a, &Obj::new()).len(), a.len());
    }

    #[test]
    fn a_change_within_the_same_second_is_still_seen() {
        // `updated_at` has one-second precision: a phase or pause change in
        // the same second leaves it equal, and must still count.
        let a = record("1-a", 0, "2026-09-29T12:00:00Z");
        let mut phase = a.clone();
        phase.insert("phase".into(), json!("huddle"));
        assert_eq!(moved(&a, &phase), ["phase"]);
        let mut paused = a.clone();
        paused.insert(
            "pauses".into(),
            json!({"codex": {"status": "rate_limited", "at": "2026-09-29T12:00:00Z"}}),
        );
        assert_eq!(moved(&a, &paused), ["pauses"]);
        let mut event = a.clone();
        event.insert("events".into(), json!([{"event": "parked"}]));
        assert_eq!(moved(&a, &event), ["events"]);
    }

    #[test]
    fn a_snapshot_retries_a_moving_record_then_says_it_is_inconsistent() {
        let dir = tempfile::tempdir().unwrap();
        let mesh = Mesh::at(dir.path(), "flag");
        // Settles on the third attempt: it is taken again, and is consistent.
        let mut n = 0;
        let settled = mesh.snapshot_reading(|_| {
            n += 1;
            Ok(Some(record(
                if n <= 3 { "x" } else { "1-a" },
                0,
                &format!("t{}", n.min(4)),
            )))
        });
        assert!(settled.consistent, "{settled:?}");
        // Never settles: after every attempt it is shown as inconsistent,
        // never as one coherent view.
        let mut m = 0;
        let moving = mesh.snapshot_reading(|_| {
            m += 1;
            Ok(Some(record("1-a", m, &format!("t{m}"))))
        });
        assert!(!moving.consistent);
        assert_eq!(moving.inconsistent, ["ownership_epoch", "updated_at"]);
        assert_eq!(usize::try_from(m).unwrap(), 2 * ATTEMPTS);
    }
}
