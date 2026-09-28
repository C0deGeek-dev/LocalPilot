//! The sending side of the push transport (spec §8b): which recipients to wake
//! after a post, the wire request, and how a reply reads. Nothing here dials:
//! the caller does the I/O under the shared deadline (P-4) and reports each
//! outcome back through [`Mesh::record_push_fact`].

use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

use super::delivery::{addressed_to, live};
use super::{participants, sid, str_of, Mesh, Obj};
use crate::jsonl;
use crate::lock::Lock;
use crate::timefmt::utc_now;

/// One deadline for all the push I/O of one post (P-4).
pub const PUSH_DEADLINE: Duration = Duration::from_secs(2);
/// The most a writer reads of one reply.
pub const PUSH_REPLY_CAP: usize = 65_536;
/// Set to `1` in the writer's environment to turn pushing off (P-3).
pub const NO_PUSH_ENV: &str = "PAIR_NO_PUSH";

/// How an endpoint is dialled (P-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// A Windows named pipe.
    Pipe,
    /// A Unix domain socket.
    Unix,
}

/// One wake to send: a message and one of its recipients' live endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushJob {
    pub session_id: String,
    /// The message's author, whose push facts record the outcome.
    pub sender: String,
    pub to: String,
    pub msg_id: String,
    pub generation: i64,
    pub transport: Transport,
    pub address: String,
}

impl PushJob {
    /// The P-2 request: one JSON object and one LF.
    #[must_use]
    pub fn request_line(&self) -> Vec<u8> {
        let req = json!({
            "v": 1,
            "op": "wake",
            "session_id": self.session_id,
            "to": self.to,
            "generation": self.generation,
            "msg_id": self.msg_id,
            "from": self.sender,
        });
        let mut line = req.to_string().into_bytes();
        line.push(b'\n');
        line
    }
}

/// Whether a push may dial this endpoint at all (P-1): a well-formed address
/// of a transport this platform provides. Anything else is never opened, so
/// an endpoint record can never make a writer open an ordinary file.
#[must_use]
pub fn dialable(transport: &str, address: &str) -> Option<Transport> {
    match transport {
        "pipe" if cfg!(windows) => {
            let name = address.strip_prefix(r"\\.\pipe\")?;
            let n = name.chars().count();
            let clean = !name
                .chars()
                .any(|c| matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|'));
            ((1..=200).contains(&n) && clean).then_some(Transport::Pipe)
        }
        "unix" if cfg!(unix) => Path::new(address).is_absolute().then_some(Transport::Unix),
        _ => None,
    }
}

/// The outcome a reply means (P-2): `sent` only for an object whose `ok` is
/// `true`; anything else, including a reply that does not parse, is a
/// refusal.
#[must_use]
pub fn outcome_of_reply(reply: &[u8]) -> &'static str {
    let line = reply.split(|b| *b == b'\n').next().unwrap_or_default();
    let ok = serde_json::from_slice::<Value>(line)
        .ok()
        .and_then(|v| v.get("ok").and_then(Value::as_bool))
        == Some(true);
    if ok {
        "sent"
    } else {
        "refused"
    }
}

impl Mesh {
    /// Queue a message this build just appended, for [`Mesh::take_push_jobs`].
    pub(crate) fn queue_push(&self, s: &Obj, m: &Obj) {
        if let Ok(mut q) = self.appended.lock() {
            q.push((s.clone(), m.clone()));
        }
    }

    /// The wakes owed for every message appended through this mailbox (or a
    /// clone of it) since the last call: one per addressed recipient, never
    /// the author, with a live endpoint whose address may be dialled and whose
    /// generation is an integer. Each recipient is decided on its own, so one
    /// unreadable endpoint costs only that recipient its wake (P-3).
    #[must_use]
    pub fn take_push_jobs(&self) -> Vec<PushJob> {
        let queued = match self.appended.lock() {
            Ok(mut q) => std::mem::take(&mut *q),
            Err(_) => return Vec::new(),
        };
        let mut jobs = Vec::new();
        for (s, m) in &queued {
            let Some(sender) = str_of(m, "role") else {
                continue;
            };
            let msg_id = str_of(m, "msg_id").map_or_else(
                || {
                    format!(
                        "{sender}:{}",
                        m.get("seq").and_then(Value::as_i64).unwrap_or(0)
                    )
                },
                str::to_owned,
            );
            for to in participants(s) {
                if to == sender || !addressed_to(m, &to) {
                    continue;
                }
                let Ok(Some(ep)) = self.read_obj(&self.mb.endpoint(sid(s), &to)) else {
                    continue;
                };
                if !live(&ep) {
                    continue;
                }
                let (Some(t), Some(address)) = (str_of(&ep, "transport"), str_of(&ep, "address"))
                else {
                    continue;
                };
                let Some(transport) = dialable(t, address) else {
                    continue;
                };
                let Some(generation) = ep.get("generation").and_then(Value::as_i64) else {
                    continue;
                };
                jobs.push(PushJob {
                    session_id: sid(s).to_owned(),
                    sender: sender.to_owned(),
                    to,
                    msg_id: msg_id.clone(),
                    generation,
                    transport,
                    address: address.to_owned(),
                });
            }
        }
        jobs
    }

    /// Record one push attempt in the sender's push facts (D-6), in the
    /// session the message was posted in. A failure to record is the
    /// caller's to ignore: nothing depends on a push (P-7).
    ///
    /// # Errors
    /// The lock or the append failing.
    pub fn record_push_fact(&self, job: &PushJob, outcome: &str) -> Result<(), crate::MeshError> {
        let _g = Lock::acquire(&self.mb.role_lock(&job.session_id, &job.sender))?;
        let rec = json!({"msg_id": job.msg_id, "to": job.to, "generation": job.generation, "at": utc_now(), "outcome": outcome});
        jsonl::append(
            &self.mb.pushes(&job.session_id, &job.sender),
            rec.as_object().unwrap_or(&Obj::new()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> PushJob {
        PushJob {
            session_id: "s".into(),
            sender: "codex".into(),
            to: "claude".into(),
            msg_id: "codex:2".into(),
            generation: 1,
            transport: Transport::Pipe,
            address: r"\\.\pipe\x".into(),
        }
    }

    #[test]
    fn the_request_is_one_json_line_with_every_field() {
        let line = job().request_line();
        assert_eq!(line.last(), Some(&b'\n'));
        assert_eq!(line.iter().filter(|b| **b == b'\n').count(), 1);
        let v: Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(
            v,
            json!({"v": 1, "op": "wake", "session_id": "s", "to": "claude", "generation": 1, "msg_id": "codex:2", "from": "codex"})
        );
    }

    #[test]
    fn only_an_ok_true_object_is_sent() {
        assert_eq!(outcome_of_reply(b"{\"ok\":true}\n"), "sent");
        assert_eq!(outcome_of_reply(b"{\"ok\": true, \"x\": 1}"), "sent");
        for r in [
            &b"{\"ok\":false,\"reason\":\"wrong_role\"}\n"[..],
            b"{\"ok\":\"true\"}\n",
            b"not json\n",
            b"",
            b"[true]\n",
        ] {
            assert_eq!(
                outcome_of_reply(r),
                "refused",
                "{:?}",
                String::from_utf8_lossy(r)
            );
        }
    }

    #[test]
    fn only_a_well_formed_address_of_this_platforms_transport_is_dialled() {
        let pipe = cfg!(windows).then_some(Transport::Pipe);
        let unix = cfg!(unix).then_some(Transport::Unix);
        assert_eq!(dialable("pipe", r"\\.\pipe\pair-x1"), pipe);
        assert_eq!(dialable("unix", "/tmp/x/wake.sock"), unix);
        for (t, a) in [
            ("pipe", "test-codex"),
            ("pipe", r"\\.\pipe\a\b"),
            ("pipe", r"\\.\pipe\"),
            ("pipe", r"\\server\pipe\x"),
            ("pipe", r"C:\repo\decoy"),
            ("unix", "relative.sock"),
            ("tcp", "127.0.0.1:1"),
        ] {
            assert_eq!(dialable(t, a), None, "{t} {a}");
        }
    }
}
