//! Delivery plumbing (spec §8a, D-1..D-8): a participant's endpoint, the
//! endpoint's receipts, and a sender's log of push attempts.
//!
//! A refusal here is exit 5 with a `REFUSED <code>: <why>` line, so an
//! adapter can act on the code; a receipt is a claim until the host binds
//! the endpoint to its process (D-8).

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::read::num_at;
use super::{participants, sid, str_of, strings, Mesh, Obj, Out};
use crate::error::MeshError;
use crate::fsio;
use crate::jsonl;
use crate::lock::Lock;
use crate::timefmt::{format_utc, parse_utc, utc_now};

/// Where an endpoint's holder presents its token: the environment, never
/// argv, which the process list shows.
pub const ENDPOINT_TOKEN_ENV: &str = "PAIR_ENDPOINT_TOKEN";

/// Outcomes a sender may record for one push attempt.
pub const PUSH_OUTCOMES: &[&str] = &["sent", "refused", "failed", "timeout"];

fn refuse(code: &str, why: &str) -> Out {
    Out {
        code: 5,
        stdout: String::new(),
        stderr: format!("REFUSED {code}: {why}\n"),
    }
}

fn now_secs() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

fn sha256_hex(s: &str) -> String {
    use std::fmt::Write as _;
    Sha256::digest(s.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// 64 hex digits from the operating system's random source.
fn new_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn active(ep: &Obj) -> bool {
    ep.get("active").and_then(Value::as_bool).unwrap_or(false)
}

/// Registered, not retired, and its lease (if any) not yet over.
pub(super) fn live(ep: &Obj) -> bool {
    let expired = str_of(ep, "expires_at")
        .filter(|e| !e.is_empty())
        .is_some_and(|e| parse_utc(e).is_none_or(|t| t <= now_secs()));
    active(ep) && !expired
}

/// The token a command-line caller presents: `PAIR_ENDPOINT_TOKEN`, never argv.
fn env_token() -> String {
    std::env::var(ENDPOINT_TOKEN_ENV).unwrap_or_default()
}

/// Whether `tok` is the token minted when `ep` was registered: `None` when
/// it is, else the refusal code.
fn token_problem(ep: &Obj, tok: &str) -> Option<&'static str> {
    if tok.is_empty() {
        return Some("no_token");
    }
    let want = str_of(ep, "token_sha256").unwrap_or_default();
    let got = sha256_hex(tok);
    let same = want.len() == got.len()
        && want
            .bytes()
            .zip(got.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0;
    (!same).then_some("bad_token")
}

/// Whether `role` is a recipient of `m`.
pub(super) fn addressed_to(m: &Obj, role: &str) -> bool {
    if str_of(m, "role") == Some(role) {
        return false;
    }
    if !m.contains_key("to") {
        return true;
    }
    strings(m.get("to")).iter().any(|r| r == role)
        || m.get("broadcast").and_then(Value::as_bool).unwrap_or(false)
}

/// What `endpoint` was asked to do.
#[derive(Debug, Clone)]
pub enum EndpointArgs {
    Register {
        transport: Option<String>,
        address: Option<String>,
        /// Seconds until the registration expires.
        ttl: Option<i64>,
    },
    Unregister,
}

/// What an endpoint operation did.
enum Done {
    Registered { generation: i64, token: String },
    Unregistered(Out),
    Refused(Out),
}

impl Mesh {
    /// `endpoint`: register or retire this participant's delivery endpoint.
    /// Generations only rise for the life of the session. The token, when one
    /// is needed, comes from `PAIR_ENDPOINT_TOKEN`.
    ///
    /// # Errors
    /// A refusal from [`Mesh::require`], or a registration missing its
    /// transport or address.
    pub fn endpoint(&self, role: &str, a: &EndpointArgs) -> Result<Out, MeshError> {
        Ok(match self.endpoint_do(role, a, &env_token(), false)? {
            // Shown once, to the adapter that registered.
            Done::Registered { generation, token } => Out::ok(format!(
                "ENDPOINT {role} generation={generation}\nENDPOINT_TOKEN={token}\n"
            )),
            Done::Unregistered(out) | Done::Refused(out) => out,
        })
    }

    /// Register (or, holding `token`, renew) this participant's endpoint from
    /// the endpoint's own process: the new generation and token come back as
    /// values and are never printed, so a listener can keep its token in
    /// memory only (spec D-8, P-6). `Err(out)` is the refusal.
    ///
    /// # Errors
    /// As [`Mesh::endpoint`].
    pub fn register_endpoint(
        &self,
        role: &str,
        transport: &str,
        address: &str,
        ttl: Option<i64>,
        token: &str,
    ) -> Result<Result<(i64, String), Out>, MeshError> {
        let a = EndpointArgs::Register {
            transport: Some(transport.to_owned()),
            address: Some(address.to_owned()),
            ttl,
        };
        Ok(match self.endpoint_do(role, &a, token, false)? {
            Done::Registered { generation, token } => Ok((generation, token)),
            Done::Unregistered(out) | Done::Refused(out) => Err(out),
        })
    }

    /// Renew an endpoint this process still holds: under the role lock, the
    /// current record must be live and registered with `token`, or nothing
    /// changes. Unlike [`Mesh::register_endpoint`], an expired record is never
    /// replaced here, however long the lock took: a lease that ran out is no
    /// longer the renewer's to keep (spec D-4 recovery stays with a fresh
    /// registration).
    ///
    /// # Errors
    /// As [`Mesh::endpoint`].
    pub fn renew_endpoint(
        &self,
        role: &str,
        transport: &str,
        address: &str,
        ttl: Option<i64>,
        token: &str,
    ) -> Result<Result<(i64, String), Out>, MeshError> {
        let a = EndpointArgs::Register {
            transport: Some(transport.to_owned()),
            address: Some(address.to_owned()),
            ttl,
        };
        Ok(match self.endpoint_do(role, &a, token, true)? {
            Done::Registered { generation, token } => Ok((generation, token)),
            Done::Unregistered(out) | Done::Refused(out) => Err(out),
        })
    }

    /// Retire this participant's endpoint, presenting `token`.
    ///
    /// # Errors
    /// As [`Mesh::endpoint`].
    pub fn unregister_endpoint(&self, role: &str, token: &str) -> Result<Out, MeshError> {
        Ok(
            match self.endpoint_do(role, &EndpointArgs::Unregister, token, false)? {
                Done::Registered { .. } => Out::ok(String::new()),
                Done::Unregistered(out) | Done::Refused(out) => out,
            },
        )
    }

    fn endpoint_do(
        &self,
        role: &str,
        a: &EndpointArgs,
        tok: &str,
        renewing: bool,
    ) -> Result<Done, MeshError> {
        let s = self.require(role, true)?;
        let p = self.mb.endpoint(sid(&s), role);
        let (gen, token) = {
            let _g = Lock::acquire(&self.mb.role_lock(sid(&s), role))?;
            let old = self.read_obj(&p)?.unwrap_or_default();
            let (transport, address, ttl) = match a {
                EndpointArgs::Unregister => {
                    if !active(&old) {
                        return Ok(Done::Refused(refuse(
                            "no_active_endpoint",
                            &format!("{role} has no active endpoint to unregister"),
                        )));
                    }
                    // An expired lease is over: retiring it needs no token.
                    if live(&old) {
                        if let Some(bad) = token_problem(&old, tok) {
                            return Ok(Done::Refused(refuse(bad, &format!("unregistering {role}'s endpoint needs the token it was registered with, in {ENDPOINT_TOKEN_ENV}"))));
                        }
                    }
                    let mut rec = old.clone();
                    rec.insert("active".into(), json!(false));
                    rec.insert("unregistered_at".into(), json!(utc_now()));
                    fsio::write_json(&p, &rec)?;
                    let gen = old
                        .get("generation")
                        .map_or_else(|| "None".to_owned(), Value::to_string);
                    return Ok(Done::Unregistered(Out::ok(format!(
                        "ENDPOINT {role} unregistered generation={gen}\n"
                    ))));
                }
                EndpointArgs::Register {
                    transport,
                    address,
                    ttl,
                } => match (
                    transport.as_deref().filter(|t| !t.is_empty()),
                    address.as_deref().filter(|a| !a.is_empty()),
                ) {
                    (Some(t), Some(ad)) => (t, ad, *ttl),
                    _ => {
                        return Err(MeshError::Refused(
                            "--register needs --transport and --address".into(),
                        ))
                    }
                },
            };
            // A renewal needs the lease still live, decided here under the
            // lock, not when the renewer last looked.
            if renewing && !live(&old) {
                return Ok(Done::Refused(refuse(
                    "expired_endpoint",
                    &format!(
                        "{role}'s endpoint is not live any more; a renewal cannot bring it back"
                    ),
                )));
            }
            // Replacing a live endpoint is rotation, and only its holder may
            // rotate it; an expired one is replaceable without its token.
            if live(&old) {
                if let Some(bad) = token_problem(&old, tok) {
                    return Ok(Done::Refused(refuse(bad, &format!("{role} already has a live endpoint; replacing it needs its token in {ENDPOINT_TOKEN_ENV}"))));
                }
            }
            let gen = num_at(&old, "generation") + 1;
            let exp = ttl.filter(|t| *t != 0).and_then(|t| {
                time::OffsetDateTime::from_unix_timestamp(now_secs() + t)
                    .ok()
                    .map(format_utc)
            });
            let token = new_token();
            fsio::write_json(
                &p,
                &json!({"participant": role, "transport": transport, "address": address,
                        "session_id": sid(&s), "generation": gen, "active": true,
                        "registered_at": utc_now(), "expires_at": exp,
                        "token_sha256": sha256_hex(&token)}),
            )?;
            (gen, token)
        };
        Ok(Done::Registered {
            generation: gen,
            token,
        })
    }

    /// `accept`: the endpoint's durable acceptance of one message, the only
    /// writer of a receipt; idempotent per message and generation.
    ///
    /// # Errors
    /// A refusal from [`Mesh::require`]; unreadable mailbox state.
    pub fn accept(&self, role: &str, msg_id: &str, generation: i64) -> Result<Out, MeshError> {
        self.accept_with(role, msg_id, generation, &env_token())
    }

    /// [`Mesh::accept`] presenting `token` directly: the endpoint's own
    /// process accepting with the token it holds in memory.
    ///
    /// # Errors
    /// As [`Mesh::accept`].
    pub fn accept_with(
        &self,
        role: &str,
        msg_id: &str,
        generation: i64,
        tok: &str,
    ) -> Result<Out, MeshError> {
        let s = self.require(role, true)?;
        {
            let _g = Lock::acquire(&self.mb.role_lock(sid(&s), role))?;
            let ep = self
                .read_obj(&self.mb.endpoint(sid(&s), role))?
                .unwrap_or_default();
            if !active(&ep) {
                return Ok(refuse(
                    "no_active_endpoint",
                    &format!("{role} has no active endpoint"),
                ));
            }
            if !live(&ep) {
                return Ok(refuse(
                    "expired_endpoint",
                    &format!(
                        "{role}'s endpoint lease expired at {}; fall back to watch",
                        str_of(&ep, "expires_at").unwrap_or("None")
                    ),
                ));
            }
            if num_at(&ep, "generation") != generation {
                let cur = ep
                    .get("generation")
                    .map_or_else(|| "None".to_owned(), Value::to_string);
                return Ok(refuse(
                    "stale_generation",
                    &format!("generation {generation} is not {role}'s current {cur}"),
                ));
            }
            if let Some(bad) = token_problem(&ep, tok) {
                return Ok(refuse(bad, &format!("only {role}'s registered endpoint may accept; it presents its token in {ENDPOINT_TOKEN_ENV}")));
            }
            let Some(m) = self.find_msg(&s, msg_id)? else {
                return Ok(refuse(
                    "unknown_message",
                    &format!("no message {msg_id} in this session"),
                ));
            };
            if !addressed_to(&m, role) {
                return Ok(refuse(
                    "not_addressed",
                    &format!("{msg_id} is not addressed to {role}"),
                ));
            }
            let rp = self.mb.receipts(sid(&s), role);
            let seen = jsonl::records(&rp)?.iter().any(|r| {
                str_of(r, "msg_id") == Some(msg_id)
                    && r.get("generation").and_then(Value::as_i64) == Some(generation)
            });
            if !seen {
                let rec = json!({"msg_id": msg_id, "generation": generation, "at": utc_now(), "fact": "accepted"});
                jsonl::append(&rp, rec.as_object().unwrap_or(&Obj::new()))?;
            }
        }
        Ok(Out::ok(format!(
            "ACCEPTED msg_id={msg_id} generation={generation}\n"
        )))
    }

    /// `record-push`: the sender's log of one push attempt. `sent` means the
    /// endpoint answered; it never means delivered or read.
    ///
    /// # Errors
    /// A refusal from [`Mesh::require`] or an unknown outcome.
    pub fn record_push(
        &self,
        role: &str,
        msg_id: &str,
        to: &str,
        generation: i64,
        outcome: &str,
    ) -> Result<Out, MeshError> {
        if !PUSH_OUTCOMES.contains(&outcome) {
            return Err(MeshError::Refused(format!("invalid outcome {outcome:?}")));
        }
        let s = self.require(role, true)?;
        if !msg_id.starts_with(&format!("{role}:")) {
            return Ok(refuse(
                "not_own_message",
                &format!("{msg_id} was not written by {role}"),
            ));
        }
        let Some(m) = self.find_msg(&s, msg_id)? else {
            return Ok(refuse(
                "unknown_message",
                &format!("no message {msg_id} in this session"),
            ));
        };
        if !participants(&s).iter().any(|p| p == to) || !addressed_to(&m, to) {
            return Ok(refuse(
                "not_addressed",
                &format!("{msg_id} is not addressed to {to}"),
            ));
        }
        {
            let _g = Lock::acquire(&self.mb.role_lock(sid(&s), role))?;
            let rec = json!({"msg_id": msg_id, "to": to, "generation": generation, "at": utc_now(), "outcome": outcome});
            jsonl::append(
                &self.mb.pushes(sid(&s), role),
                rec.as_object().unwrap_or(&Obj::new()),
            )?;
        }
        Ok(Out::ok(format!(
            "PUSH_RECORDED msg_id={msg_id} to={to} outcome={outcome}\n"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_64_hex_digits_and_hash_to_64() {
        let t = new_token();
        assert_eq!(t.len(), 64);
        assert!(t.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn a_historic_message_is_addressed_to_everyone_but_its_author() {
        let m: Obj = serde_json::from_value(json!({"role": "claude"})).unwrap();
        assert!(addressed_to(&m, "codex"));
        assert!(!addressed_to(&m, "claude"));
        let d: Obj = serde_json::from_value(json!({"role": "claude", "to": ["codex"]})).unwrap();
        assert!(!addressed_to(&d, "localpilot"));
    }
}
