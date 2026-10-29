//! Idempotent retries for mutating commands on `localpilot serve`.
//!
//! A client that loses a reply cannot tell whether its command ran. A command
//! that carries a request key — `{instance, client, n}` — can be retried
//! safely: `instance` names the server process that issued the client's
//! `attached` (a restarted server has another), `client` is an id the client
//! chooses, and `n` only increases per client. Per session, principal and
//! client the server keeps the highest `n` it admitted and the replies of the
//! most recent requests:
//!
//! - `n` above the highest: a new request. It is reserved as pending before it
//!   runs, so a duplicate arriving meanwhile is refused as `in_progress`.
//! - `n` at or below the highest, still remembered: the same payload gets the
//!   original reply again; another payload is refused as `id_reused`.
//! - `n` at or below the highest, forgotten, or another server instance:
//!   refused as `outcome_unknown`, with no effect. It is never run as new.
//!
//! The store lives beside the host map, keyed by session, so it survives a
//! session being closed after inactivity and resumed within the same server.
//! A client's stream is never forgotten while the server runs; a session
//! accepts a bounded number of client streams and refuses more.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, PoisonError};

use localpilot_core::{EventId, SessionId};
use localpilot_rpc::{ClientCommand, RejectCode, RequestKey, ServerEvent};

/// Replies remembered per client stream.
const WINDOW: usize = 128;
/// Client streams per session.
const MAX_STREAMS: usize = 64;

/// What to do with a request that carries a key.
#[derive(Debug, PartialEq)]
pub(crate) enum Admission {
    /// Run it, then [`RequestStore::complete`] it with its reply bytes.
    Run,
    /// Send these replies again (under the retry's own correlation id); do
    /// not run anything.
    Replay(Vec<ServerEvent>),
    /// Refuse without effect.
    Refuse(RejectCode, String),
}

enum Entry {
    Pending {
        payload: Vec<u8>,
    },
    Done {
        payload: Vec<u8>,
        reply: Vec<ServerEvent>,
    },
}

#[derive(Default)]
struct Stream {
    highest: u64,
    window: BTreeMap<u64, Entry>,
}

/// Every session's request streams, for one server process.
pub(crate) struct RequestStore {
    instance: String,
    streams: Mutex<HashMap<(SessionId, String, String), Stream>>,
}

impl Default for RequestStore {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestStore {
    pub(crate) fn new() -> Self {
        Self {
            instance: EventId::new().to_string(),
            streams: Mutex::new(HashMap::new()),
        }
    }

    /// This server process's instance id, advertised in `attached`.
    pub(crate) fn instance(&self) -> &str {
        &self.instance
    }

    /// Decide what to do with `key` for `payload` (the command's canonical
    /// bytes, compared exactly), and reserve it when it is new. One lock covers
    /// the decision and the reservation, so two copies of a request never both
    /// run.
    pub(crate) fn admit(
        &self,
        session: SessionId,
        principal: &str,
        key: &RequestKey,
        payload: &[u8],
    ) -> Admission {
        if key.instance != self.instance {
            return Admission::Refuse(
                RejectCode::OutcomeUnknown,
                "this request was issued to an earlier server instance; its outcome is unknown"
                    .to_string(),
            );
        }
        let mut streams = self.streams.lock().unwrap_or_else(PoisonError::into_inner);
        let stream_key = (session, principal.to_string(), key.client.clone());
        if !streams.contains_key(&stream_key) {
            let in_session = streams.keys().filter(|(id, _, _)| *id == session).count();
            if in_session >= MAX_STREAMS {
                return Admission::Refuse(
                    RejectCode::TooManyClients,
                    "this session already tracks the maximum number of client request streams"
                        .to_string(),
                );
            }
        }
        let stream = streams.entry(stream_key).or_default();
        if key.n > stream.highest {
            // Make room by forgetting the oldest finished request. A pending
            // one is never forgotten: if every remembered request is still
            // pending, the new one is refused without effect.
            if stream.window.len() >= WINDOW {
                let oldest_done = stream
                    .window
                    .iter()
                    .find(|(_, entry)| matches!(entry, Entry::Done { .. }))
                    .map(|(n, _)| *n);
                match oldest_done {
                    Some(n) => {
                        stream.window.remove(&n);
                    }
                    None => {
                        return Admission::Refuse(
                            RejectCode::InProgress,
                            "too many of this client's requests are still being handled"
                                .to_string(),
                        );
                    }
                }
            }
            stream.highest = key.n;
            stream.window.insert(
                key.n,
                Entry::Pending {
                    payload: payload.to_vec(),
                },
            );
            return Admission::Run;
        }
        match stream.window.get(&key.n) {
            Some(Entry::Pending { payload: seen }) | Some(Entry::Done { payload: seen, .. })
                if seen.as_slice() != payload =>
            {
                Admission::Refuse(
                    RejectCode::IdReused,
                    format!("request {} was already used for a different command", key.n),
                )
            }
            Some(Entry::Pending { .. }) => Admission::Refuse(
                RejectCode::InProgress,
                format!("request {} is still being handled", key.n),
            ),
            Some(Entry::Done { reply, .. }) => Admission::Replay(reply.clone()),
            None => Admission::Refuse(
                RejectCode::OutcomeUnknown,
                format!(
                    "request {} is older than this server remembers; its outcome is unknown",
                    key.n
                ),
            ),
        }
    }

    /// Record the reply of a request [`admit`](Self::admit) told the caller to
    /// run.
    pub(crate) fn complete(
        &self,
        session: SessionId,
        principal: &str,
        key: &RequestKey,
        reply: Vec<ServerEvent>,
    ) {
        let mut streams = self.streams.lock().unwrap_or_else(PoisonError::into_inner);
        let stream_key = (session, principal.to_string(), key.client.clone());
        if let Some(stream) = streams.get_mut(&stream_key) {
            if let Some(entry) = stream.window.get_mut(&key.n) {
                if let Entry::Pending { payload } = entry {
                    let payload = std::mem::take(payload);
                    *entry = Entry::Done { payload, reply };
                }
            }
        }
    }
}

/// The canonical bytes of a typed command, binding a request key to exactly
/// what it asked for. The correlation id and the key itself are not part of
/// them, so a retry under a new correlation id still matches.
///
/// # Errors
/// Returns the serialization error, which the caller must refuse rather than
/// treat as some default payload.
pub(crate) fn canonical_payload(command: &ClientCommand) -> serde_json::Result<Vec<u8>> {
    serde_json::to_vec(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A payload standing for one distinct command.
    fn p(n: u64) -> Vec<u8> {
        n.to_le_bytes().to_vec()
    }

    fn key(store: &RequestStore, client: &str, n: u64) -> RequestKey {
        RequestKey {
            instance: store.instance().to_string(),
            client: client.to_string(),
            n,
        }
    }

    #[test]
    fn a_new_request_runs_once_and_a_retry_replays_its_reply() {
        let store = RequestStore::new();
        let session = SessionId::new();
        let k = key(&store, "phone", 1);
        assert_eq!(store.admit(session, "local", &k, &p(7)), Admission::Run);
        // A duplicate before the reply is recorded does not run.
        assert!(matches!(
            store.admit(session, "local", &k, &p(7)),
            Admission::Refuse(RejectCode::InProgress, _)
        ));
        store.complete(session, "local", &k, vec![ServerEvent::Closed]);
        assert_eq!(
            store.admit(session, "local", &k, &p(7)),
            Admission::Replay(vec![ServerEvent::Closed])
        );
        assert!(matches!(
            store.admit(session, "local", &k, &p(8)),
            Admission::Refuse(RejectCode::IdReused, _)
        ));
    }

    #[test]
    fn a_forgotten_or_foreign_request_is_never_run_as_new() {
        let store = RequestStore::new();
        let session = SessionId::new();
        for n in 1..=(WINDOW as u64 + 1) {
            let k = key(&store, "phone", n);
            assert_eq!(store.admit(session, "local", &k, &p(n)), Admission::Run);
            store.complete(session, "local", &k, Vec::new());
        }
        // Request 1 fell out of the window: unknown, never re-run.
        assert!(matches!(
            store.admit(session, "local", &key(&store, "phone", 1), &p(1)),
            Admission::Refuse(RejectCode::OutcomeUnknown, _)
        ));
        // A request issued to another server instance.
        let foreign = RequestKey {
            instance: "an-earlier-server".to_string(),
            client: "phone".to_string(),
            n: 999,
        };
        assert!(matches!(
            store.admit(session, "local", &foreign, &p(1)),
            Admission::Refuse(RejectCode::OutcomeUnknown, _)
        ));
    }

    #[test]
    fn the_payload_is_the_exact_typed_command() {
        use localpilot_rpc::InputDisposition;
        let prompt = |text: &str, disposition| ClientCommand::Prompt {
            text: text.to_string(),
            disposition,
        };
        let same = canonical_payload(&prompt("go", InputDisposition::Immediate)).unwrap();
        assert_eq!(
            same,
            canonical_payload(&prompt("go", InputDisposition::Immediate)).unwrap()
        );
        for other in [
            prompt("go!", InputDisposition::Immediate),
            prompt("go", InputDisposition::FollowUp),
            ClientCommand::Cancel,
        ] {
            assert_ne!(same, canonical_payload(&other).unwrap());
        }
        let reply = |allow| ClientCommand::PermissionReply {
            ask_id: "ask-1".to_string(),
            allow,
        };
        assert_ne!(
            canonical_payload(&reply(true)).unwrap(),
            canonical_payload(&reply(false)).unwrap()
        );
    }

    #[test]
    fn a_pending_request_is_never_forgotten_to_make_room() {
        let store = RequestStore::new();
        let session = SessionId::new();
        for n in 1..=WINDOW as u64 {
            assert_eq!(
                store.admit(session, "local", &key(&store, "phone", n), &p(n)),
                Admission::Run
            );
        }
        // Every remembered request is still pending: a new one is refused...
        let next = key(&store, "phone", WINDOW as u64 + 1);
        assert!(matches!(
            store.admit(session, "local", &next, &p(0)),
            Admission::Refuse(RejectCode::InProgress, _)
        ));
        // ...and was not admitted, so it runs once room is made.
        store.complete(session, "local", &key(&store, "phone", 1), Vec::new());
        assert_eq!(store.admit(session, "local", &next, &p(0)), Admission::Run);
        // Request 2 is still pending and still known.
        assert!(matches!(
            store.admit(session, "local", &key(&store, "phone", 2), &p(2)),
            Admission::Refuse(RejectCode::InProgress, _)
        ));
    }

    #[test]
    fn streams_are_per_session_principal_and_client_and_bounded() {
        let store = RequestStore::new();
        let session = SessionId::new();
        let other = SessionId::new();
        assert_eq!(
            store.admit(session, "local", &key(&store, "a", 1), &p(1)),
            Admission::Run
        );
        // The same n from another client, or in another session, is new.
        assert_eq!(
            store.admit(session, "local", &key(&store, "b", 1), &p(1)),
            Admission::Run
        );
        assert_eq!(
            store.admit(other, "local", &key(&store, "a", 1), &p(1)),
            Admission::Run
        );
        for n in 2..MAX_STREAMS {
            let client = format!("c{n}");
            assert_eq!(
                store.admit(session, "local", &key(&store, &client, 1), &p(1)),
                Admission::Run
            );
        }
        assert!(matches!(
            store.admit(session, "local", &key(&store, "one-too-many", 1), &p(1)),
            Admission::Refuse(RejectCode::TooManyClients, _)
        ));
        // Existing streams keep working when the session is full.
        assert_eq!(
            store.admit(session, "local", &key(&store, "a", 2), &p(2)),
            Admission::Run
        );
    }
}
