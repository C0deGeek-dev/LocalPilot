//! Behaviour tests for the peer-pair convergence protocol. Split out of
//! `converge.rs` to keep the production module compact; a child module, so
//! `use super::*` reaches the crate-internal protocol/driver items.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
fn session_directive_uses_the_parsers_version_and_preserves_identity_and_task() {
    let task = "  preserve these task bytes\r\nincluding the edges  ";
    let directive = pair_session_directive("A", "B", task);

    assert!(directive.starts_with("You are peer A in a symmetric two-peer collaboration."));
    assert!(directive.contains("Peer B is working on the same original task."));
    assert!(directive.contains(&format!(
        "{{\"v\":{PROTOCOL_VERSION},\"action\":\"propose\""
    )));
    assert!(directive.contains(&format!("{{\"v\":{PROTOCOL_VERSION},\"action\":\"revise\"")));
    assert!(directive.contains(&format!("{{\"v\":{PROTOCOL_VERSION},\"action\":\"agree\"")));
    assert!(directive.contains(&format!("Original task:\n{task}\n\nOn every scheduled")));
    assert!(!directive.contains("coordinator"));
}

#[test]
fn a_well_formed_envelope_parses_to_each_action() {
    assert_eq!(
        parse_action(r#"{"v":1,"action":"propose","artifact":"first cut"}"#).unwrap(),
        PairAction::Propose {
            artifact: "first cut".to_string()
        }
    );
    assert_eq!(
        parse_action(r#"{"v":1,"action":"revise","artifact":"second cut"}"#).unwrap(),
        PairAction::Revise {
            artifact: "second cut".to_string()
        }
    );
    assert_eq!(
        parse_action(r#"{"v":1,"action":"agree","revision":3,"digest":"abc123"}"#).unwrap(),
        PairAction::Agree {
            revision: 3,
            digest: "abc123".to_string()
        }
    );
}

#[test]
fn a_future_version_is_refused_rather_than_misread() {
    let error = parse_action(r#"{"v":2,"action":"propose","artifact":"x"}"#).unwrap_err();
    assert_eq!(
        error,
        EnvelopeError::UnknownVersion {
            found: 2,
            expected: PROTOCOL_VERSION
        }
    );
}

#[test]
fn non_json_is_malformed_not_a_panic() {
    assert!(matches!(
        parse_action("not an envelope"),
        Err(EnvelopeError::Malformed(_))
    ));
    assert!(matches!(parse_action(""), Err(EnvelopeError::Malformed(_))));
}

#[test]
fn a_missing_required_field_is_malformed() {
    // `propose` with no `artifact`.
    assert!(matches!(
        parse_action(r#"{"v":1,"action":"propose"}"#),
        Err(EnvelopeError::Malformed(_))
    ));
    // `agree` missing its digest.
    assert!(matches!(
        parse_action(r#"{"v":1,"action":"agree","revision":1}"#),
        Err(EnvelopeError::Malformed(_))
    ));
}

#[test]
fn an_unknown_action_is_malformed() {
    assert!(matches!(
        parse_action(r#"{"v":1,"action":"concede","artifact":"x"}"#),
        Err(EnvelopeError::Malformed(_))
    ));
}

#[test]
fn a_spoofed_identity_field_is_ignored_and_leaves_no_trace() {
    // A peer cannot smuggle "who I am" into the protocol: extra fields are
    // dropped, and the parsed action carries no identity for them to land in.
    let action = parse_action(
            r#"{"v":1,"action":"propose","artifact":"x","session":"some-other-peer","from":"impostor"}"#,
        )
        .unwrap();
    assert_eq!(
        action,
        PairAction::Propose {
            artifact: "x".to_string()
        }
    );
}

// ── Candidate state, digest, and agreement ────────────────────────────────

fn pair() -> (SessionId, SessionId, PairState) {
    let a = SessionId::new();
    let b = SessionId::new();
    // Fresh ids are distinct; `PairDriver::new` is what enforces that in public.
    (a, b, PairState::distinct(a, b))
}

/// The revision and digest the current candidate carries, for an `Agree`.
fn current(state: &PairState) -> (u64, String) {
    let candidate = state.candidate().expect("a candidate is installed");
    (candidate.revision(), candidate.digest().to_string())
}

#[test]
fn canonicalize_normalises_every_line_ending_to_lf() {
    assert_eq!(canonicalize("a\r\nb\rc\n"), "a\nb\nc\n");
    // Bytes other than line endings — including trailing spaces — are kept.
    assert_eq!(canonicalize("keep  \tme"), "keep  \tme");
}

#[test]
fn the_digest_is_real_sha256_over_the_canonical_bytes() {
    // Pinned against the published SHA-256 of "abc", so this is provably the
    // real hash and not some look-alike.
    assert_eq!(
        digest_of("abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    // CRLF vs LF is invisible once canonicalised: the two digest the same, so
    // two peers on different platforms agree on identical content.
    assert_eq!(
        digest_of(&canonicalize("line1\r\nline2")),
        digest_of(&canonicalize("line1\nline2"))
    );
}

#[test]
fn a_first_propose_installs_revision_one_with_a_digest() {
    let (a, _b, mut state) = pair();
    assert_eq!(
        state
            .apply(
                a,
                PairAction::Propose {
                    artifact: "hello".to_string()
                }
            )
            .unwrap(),
        Transition::CandidateInstalled { revision: 1 }
    );
    let candidate = state.candidate().expect("installed");
    assert_eq!(candidate.revision(), 1);
    assert_eq!(candidate.digest(), digest_of("hello"));
    assert_eq!(candidate.artifact(), "hello");
}

#[test]
fn both_peers_agreeing_the_current_candidate_converges() {
    let (a, b, mut state) = pair();
    state
        .apply(
            a,
            PairAction::Propose {
                artifact: "final".to_string(),
            },
        )
        .unwrap();
    let (revision, digest) = current(&state);
    assert_eq!(
        state
            .apply(
                b,
                PairAction::Agree {
                    revision,
                    digest: digest.clone()
                }
            )
            .unwrap(),
        Transition::AgreementRecorded { revision: 1 }
    );
    assert_eq!(
        state
            .apply(a, PairAction::Agree { revision, digest })
            .unwrap(),
        Transition::Converged { revision: 1 }
    );
}

#[test]
fn one_peer_agreeing_twice_does_not_converge_for_the_pair() {
    let (a, _b, mut state) = pair();
    state
        .apply(
            a,
            PairAction::Propose {
                artifact: "solo".to_string(),
            },
        )
        .unwrap();
    let (revision, digest) = current(&state);
    assert_eq!(
        state
            .apply(
                a,
                PairAction::Agree {
                    revision,
                    digest: digest.clone()
                }
            )
            .unwrap(),
        Transition::AgreementRecorded { revision: 1 }
    );
    // The same peer agreeing again is idempotent — still just one peer.
    assert_eq!(
        state
            .apply(a, PairAction::Agree { revision, digest })
            .unwrap(),
        Transition::AgreementRecorded { revision: 1 }
    );
}

#[test]
fn a_revise_clears_prior_agreements_and_supersedes_the_revision() {
    let (a, b, mut state) = pair();
    state
        .apply(
            a,
            PairAction::Propose {
                artifact: "v1".to_string(),
            },
        )
        .unwrap();
    let (rev1, digest1) = current(&state);
    state
        .apply(
            b,
            PairAction::Agree {
                revision: rev1,
                digest: digest1.clone(),
            },
        )
        .unwrap();

    // A revise installs revision 2 and wipes B's agreement.
    assert_eq!(
        state
            .apply(
                a,
                PairAction::Revise {
                    artifact: "v2".to_string()
                }
            )
            .unwrap(),
        Transition::CandidateInstalled { revision: 2 }
    );

    // B's now-stale agreement to revision 1 counts for nothing.
    assert_eq!(
        state
            .apply(
                b,
                PairAction::Agree {
                    revision: rev1,
                    digest: digest1
                }
            )
            .unwrap(),
        Transition::StaleAgreement
    );

    // Both must agree the current revision 2 to converge.
    let (rev2, digest2) = current(&state);
    state
        .apply(
            a,
            PairAction::Agree {
                revision: rev2,
                digest: digest2.clone(),
            },
        )
        .unwrap();
    assert_eq!(
        state
            .apply(
                b,
                PairAction::Agree {
                    revision: rev2,
                    digest: digest2
                }
            )
            .unwrap(),
        Transition::Converged { revision: 2 }
    );
}

#[test]
fn an_agree_with_the_wrong_revision_or_digest_is_stale() {
    let (a, b, mut state) = pair();
    // Agree before any candidate is stale, never a convergence.
    assert_eq!(
        state
            .apply(
                a,
                PairAction::Agree {
                    revision: 1,
                    digest: "whatever".to_string()
                }
            )
            .unwrap(),
        Transition::StaleAgreement
    );

    state
        .apply(
            a,
            PairAction::Propose {
                artifact: "the thing".to_string(),
            },
        )
        .unwrap();
    let (revision, digest) = current(&state);

    // Right digest, wrong revision.
    assert_eq!(
        state
            .apply(
                b,
                PairAction::Agree {
                    revision: revision + 5,
                    digest: digest.clone()
                }
            )
            .unwrap(),
        Transition::StaleAgreement
    );
    // Right revision, wrong digest — the cross-check catches a peer that names
    // a revision it did not actually see.
    assert_eq!(
        state
            .apply(
                b,
                PairAction::Agree {
                    revision,
                    digest: "0000000000000000000000000000000000000000000000000000000000000000"
                        .to_string()
                }
            )
            .unwrap(),
        Transition::StaleAgreement
    );
}

#[test]
fn repeated_revises_bump_the_revision_monotonically() {
    let (a, b, mut state) = pair();
    for (turn, expected) in [(a, 1u64), (b, 2), (a, 3)] {
        assert_eq!(
            state
                .apply(
                    turn,
                    PairAction::Revise {
                        artifact: format!("draft {expected}")
                    }
                )
                .unwrap(),
            Transition::CandidateInstalled { revision: expected }
        );
    }
    assert_eq!(state.candidate().expect("installed").revision(), 3);
}

#[test]
fn a_revision_counter_at_the_ceiling_is_refused_without_mutation() {
    let (a, _b, mut state) = pair();
    // Seat the counter at the ceiling directly — a loop to reach it is not
    // feasible, and the boundary is what matters.
    state.next_revision = u64::MAX;
    assert_eq!(
        state
            .apply(
                a,
                PairAction::Propose {
                    artifact: "overflow".to_string()
                }
            )
            .unwrap_err(),
        PairStateError::RevisionExhausted
    );
    assert!(
        state.candidate().is_none(),
        "an exhausted revision must not install a candidate"
    );
}

#[test]
fn an_action_from_a_session_outside_the_pair_is_refused() {
    let (_a, _b, mut state) = pair();
    let outsider = SessionId::new();
    assert_eq!(
        state
            .apply(
                outsider,
                PairAction::Propose {
                    artifact: "intrusion".to_string()
                }
            )
            .unwrap_err(),
        PairStateError::ForeignActor
    );
    assert!(
        state.candidate().is_none(),
        "a foreign action must not touch the shared state"
    );
}

// ── Driver: scheduling, delivery, and repair ──────────────────────────────

use std::collections::{HashMap, VecDeque};

/// A scripted move a fake peer makes on its turn.
#[derive(Debug, Clone)]
enum Move {
    Propose(String),
    Revise(String),
    /// Agree to whatever was last delivered to this peer.
    AgreeLatest,
    /// Agree to the delivered revision but with a wrong digest (a stale result).
    AgreeWrongDigest,
    /// Return text that is not an envelope at all (cost 1).
    Malformed,
    /// Park until the driver's cancellation signal fires, then stop cleanly.
    Hang,
    /// The session fails to run its turn.
    PeerFail,
    /// The provider errors.
    ProviderFail,
    /// A valid proposal that costs this many tokens.
    Costly(u64),
    /// A malformed reply that still costs this many tokens.
    CostlyBad(u64),
    /// Sleep this many milliseconds, then return a malformed reply (cost 1).
    SlowBad(u64),
    /// Sleep this many milliseconds, then return a valid proposal — ignoring the
    /// cancel token entirely (a cancellation-oblivious endpoint).
    SlowProduced(u64),
    /// Sleep this many milliseconds, then return a valid revision.
    SlowRevise(u64),
    /// Abort the pair from inside this drive, then still return a valid proposal —
    /// a cancellation-oblivious endpoint whose reply lands after the abort.
    AbortThenProduce(PairAbort),
    /// Report a clean cancellation though the driver set no signal — an anomaly.
    SpuriousCancel,
    /// The session reports its own timeout / budget / no-progress stop.
    SessionTimedOut,
    SessionBudget,
    SessionNoProgress,
}

/// How the fake's delivery behaves.
#[derive(Clone)]
enum NotifyBehavior {
    Deliver,
    Fail(EndpointError),
    /// Park until the cancellation signal fires, then stop cleanly.
    Hang,
    /// Abort the pair from inside this delivery, then stop cleanly — the abort
    /// lands after the prior slot's candidate was applied.
    AbortThenHang(PairAbort),
}

/// One recorded endpoint call, so a test can assert ordering.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    Notify { from: SessionId, to: SessionId },
    Drive { peer: SessionId, prompt: String },
}

/// Deterministic fake endpoints: each peer plays a scripted list of moves, and
/// every call is logged. `notify` remembers what was delivered so an
/// `AgreeLatest` can echo the right revision and digest.
struct FakeEndpoints {
    scripts: HashMap<SessionId, VecDeque<Move>>,
    delivered: HashMap<SessionId, (u64, String)>,
    log: Vec<Call>,
    notify_behavior: NotifyBehavior,
}

impl FakeEndpoints {
    fn new(scripts: HashMap<SessionId, VecDeque<Move>>) -> Self {
        Self {
            scripts,
            delivered: HashMap::new(),
            log: Vec::new(),
            notify_behavior: NotifyBehavior::Deliver,
        }
    }

    fn with_notify(mut self, behavior: NotifyBehavior) -> Self {
        self.notify_behavior = behavior;
        self
    }

    fn drives(&self, peer: SessionId) -> usize {
        self.log
            .iter()
            .filter(|call| matches!(call, Call::Drive { peer: p, .. } if *p == peer))
            .count()
    }

    fn notifies(&self, peer: SessionId) -> usize {
        self.log
            .iter()
            .filter(|call| matches!(call, Call::Notify { to, .. } if *to == peer))
            .count()
    }
}

/// Pull the revision and digest out of a rendered delivery.
fn parse_delivery(content: &str) -> (u64, String) {
    let revision = content
        .split("revision ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|word| word.parse().ok())
        .expect("a revision in the delivery");
    let digest = content
        .split("digest ")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .map(|word| word.trim().to_string())
        .expect("a digest in the delivery");
    (revision, digest)
}

fn produced(envelope: String) -> Result<TurnReply, EndpointError> {
    Ok(TurnReply::Produced { envelope, cost: 1 })
}

fn bad(cost: u64) -> Result<TurnReply, EndpointError> {
    Ok(TurnReply::Produced {
        envelope: "this is not an envelope".to_string(),
        cost,
    })
}

impl PairEndpoints for FakeEndpoints {
    async fn notify(
        &mut self,
        from: SessionId,
        to: SessionId,
        content: &str,
        cancel: &CancellationToken,
    ) -> Result<NotifyReply, EndpointError> {
        self.log.push(Call::Notify { from, to });
        match self.notify_behavior.clone() {
            NotifyBehavior::Deliver => {
                self.delivered.insert(to, parse_delivery(content));
                Ok(NotifyReply::Delivered)
            }
            NotifyBehavior::Fail(error) => Err(error),
            NotifyBehavior::Hang => {
                cancel.cancelled().await;
                Ok(NotifyReply::Cancelled)
            }
            NotifyBehavior::AbortThenHang(abort) => {
                abort.abort();
                cancel.cancelled().await;
                Ok(NotifyReply::Cancelled)
            }
        }
    }

    async fn drive(
        &mut self,
        peer: SessionId,
        prompt: &str,
        cancel: &CancellationToken,
    ) -> Result<TurnReply, EndpointError> {
        self.log.push(Call::Drive {
            peer,
            prompt: prompt.to_string(),
        });
        let next = self
            .scripts
            .get_mut(&peer)
            .and_then(VecDeque::pop_front)
            .expect("a scripted move for the driven peer");
        match next {
            Move::Propose(artifact) => produced(
                serde_json::json!({"v":1,"action":"propose","artifact":artifact}).to_string(),
            ),
            Move::Revise(artifact) => produced(
                serde_json::json!({"v":1,"action":"revise","artifact":artifact}).to_string(),
            ),
            Move::AgreeLatest => {
                let (revision, digest) = self.delivered.get(&peer).cloned().expect("a delivery");
                produced(
                    serde_json::json!({"v":1,"action":"agree","revision":revision,"digest":digest})
                        .to_string(),
                )
            }
            Move::AgreeWrongDigest => {
                let (revision, _) = self.delivered.get(&peer).cloned().expect("a delivery");
                produced(
                        serde_json::json!({"v":1,"action":"agree","revision":revision,"digest":"0".repeat(64)})
                            .to_string(),
                    )
            }
            Move::Malformed => bad(1),
            Move::Hang => {
                // Park exactly as a real turn parks on its own cancellation, then
                // report the clean stop — the driver never dropped this future.
                cancel.cancelled().await;
                Ok(TurnReply::Cancelled)
            }
            Move::PeerFail => Err(EndpointError::PeerFailed("session lost".to_string())),
            Move::ProviderFail => Err(EndpointError::ProviderError(
                "model unavailable".to_string(),
            )),
            Move::Costly(cost) => Ok(TurnReply::Produced {
                envelope: serde_json::json!({"v":1,"action":"propose","artifact":"expensive"})
                    .to_string(),
                cost,
            }),
            Move::CostlyBad(cost) => bad(cost),
            Move::SlowBad(ms) => {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                bad(1)
            }
            Move::SlowProduced(ms) => {
                // Deliberately ignores `cancel`: a result lands after the sleep
                // whether or not the driver signalled, so the driver's own
                // deadline handling is what must decide the outcome.
                tokio::time::sleep(Duration::from_millis(ms)).await;
                produced(
                    serde_json::json!({"v":1,"action":"propose","artifact":"late"}).to_string(),
                )
            }
            Move::SlowRevise(ms) => {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                produced(
                    serde_json::json!({"v":1,"action":"revise","artifact":"slow-revise"})
                        .to_string(),
                )
            }
            Move::AbortThenProduce(abort) => {
                abort.abort();
                produced(
                    serde_json::json!({"v":1,"action":"propose","artifact":"late"}).to_string(),
                )
            }
            Move::SpuriousCancel => Ok(TurnReply::Cancelled),
            Move::SessionTimedOut => Ok(TurnReply::TimedOut),
            Move::SessionBudget => Ok(TurnReply::BudgetExceeded),
            Move::SessionNoProgress => Ok(TurnReply::NoProgress),
        }
    }
}

fn scripts(a: SessionId, a_moves: &[Move], b: SessionId, b_moves: &[Move]) -> FakeEndpoints {
    let mut map = HashMap::new();
    map.insert(a, a_moves.iter().cloned().collect());
    map.insert(b, b_moves.iter().cloned().collect());
    FakeEndpoints::new(map)
}

/// Generous default bounds: enough rounds and time that only the scripted moves
/// decide the outcome; the slot budget is off.
fn bounds() -> PairBounds {
    PairBounds {
        max_rounds: 5,
        slot_timeout: Duration::from_secs(3600),
        slot_token_budget: 0,
    }
}

/// Run to completion and return just the terminal reason — most tests assert
/// only that; the retention and progress tests use the full report or receiver.
async fn run_reason(driver: PairDriver, endpoints: &mut FakeEndpoints) -> PairOutcome {
    driver.run(endpoints).await.reason().clone()
}

#[tokio::test]
async fn the_pair_converges_over_a_scripted_exchange() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(a, b, "solve it", bounds()).unwrap();
    let mut endpoints = scripts(
        a,
        &[Move::Propose("draft".to_string()), Move::AgreeLatest],
        b,
        &[Move::AgreeLatest],
    );
    assert_eq!(
        run_reason(driver, &mut endpoints).await,
        PairOutcome::Converged { revision: 1 }
    );
}

#[tokio::test]
async fn a_delivery_is_notified_before_the_recipient_is_driven() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(a, b, "task", bounds()).unwrap();
    let mut endpoints = scripts(
        a,
        &[Move::Propose("d".to_string()), Move::AgreeLatest],
        b,
        &[Move::AgreeLatest],
    );
    run_reason(driver, &mut endpoints).await;

    // The first turn (A) is driven with no delivery before it.
    assert_eq!(
        endpoints.log[0],
        Call::Drive {
            peer: a,
            prompt: "task".to_string()
        }
    );
    // B's turn is delivered to (from A), then B is driven — in that order.
    let notify_b = endpoints
        .log
        .iter()
        .position(|call| matches!(call, Call::Notify { from, to } if *from == a && *to == b))
        .expect("B was notified from A");
    let drive_b = endpoints
        .log
        .iter()
        .position(|call| matches!(call, Call::Drive { peer, .. } if *peer == b))
        .expect("B was driven");
    assert!(
        notify_b < drive_b,
        "notify must precede drive: {:?}",
        endpoints.log
    );
    // Later, A is delivered to from B (the counterpart), never from itself.
    assert!(
        endpoints
            .log
            .iter()
            .any(|call| matches!(call, Call::Notify { from, to } if *from == b && *to == a)),
        "A is delivered to from B: {:?}",
        endpoints.log
    );
    // Nothing is delivered before the very first drive.
    assert!(!matches!(endpoints.log[0], Call::Notify { .. }));
}

#[tokio::test]
async fn the_first_turn_uses_the_task_and_later_turns_use_the_pair_prompt() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(a, b, "the original task", bounds()).unwrap();
    let mut endpoints = scripts(
        a,
        &[Move::Propose("d".to_string()), Move::AgreeLatest],
        b,
        &[Move::AgreeLatest],
    );
    run_reason(driver, &mut endpoints).await;

    let prompts: Vec<&str> = endpoints
        .log
        .iter()
        .filter_map(|call| match call {
            Call::Drive { prompt, .. } => Some(prompt.as_str()),
            Call::Notify { .. } => None,
        })
        .collect();
    assert_eq!(prompts[0], "the original task");
    assert!(
        prompts[1..]
            .iter()
            .all(|prompt| *prompt == PAIR_TURN_PROMPT),
        "every later turn uses the pair prompt: {prompts:?}"
    );
}

#[tokio::test]
async fn a_bad_result_is_repaired_on_the_same_peer_without_re_delivering() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(a, b, "task", bounds()).unwrap();
    // B's first result is malformed; the repair re-drives B and it then agrees.
    let mut endpoints = scripts(
        a,
        &[Move::Propose("d".to_string()), Move::AgreeLatest],
        b,
        &[Move::Malformed, Move::AgreeLatest],
    );
    assert_eq!(
        run_reason(driver, &mut endpoints).await,
        PairOutcome::Converged { revision: 1 }
    );
    // B was driven twice (original + repair) but delivered to only once.
    assert_eq!(endpoints.drives(b), 2, "B was re-driven for the repair");
    assert_eq!(
        endpoints.notifies(b),
        1,
        "the repair must not re-deliver to B"
    );
}

#[tokio::test]
async fn two_bad_results_in_one_slot_end_the_pair() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(a, b, "task", bounds()).unwrap();
    let mut endpoints = scripts(
        a,
        &[Move::Propose("d".to_string())],
        b,
        &[Move::Malformed, Move::Malformed],
    );
    assert!(matches!(
        run_reason(driver, &mut endpoints).await,
        PairOutcome::ProtocolError { .. }
    ));
}

#[tokio::test]
async fn a_stale_agreement_draws_on_the_same_repair_budget() {
    let a = SessionId::new();
    let b = SessionId::new();
    // Stale-then-good repairs and converges.
    let driver = PairDriver::new(a, b, "task", bounds()).unwrap();
    let mut endpoints = scripts(
        a,
        &[Move::Propose("d".to_string()), Move::AgreeLatest],
        b,
        &[Move::AgreeWrongDigest, Move::AgreeLatest],
    );
    assert_eq!(
        run_reason(driver, &mut endpoints).await,
        PairOutcome::Converged { revision: 1 }
    );

    // Two stale results in the same slot end the pair, exactly like two
    // malformed ones.
    let driver = PairDriver::new(a, b, "task", bounds()).unwrap();
    let mut endpoints = scripts(
        a,
        &[Move::Propose("d".to_string())],
        b,
        &[Move::AgreeWrongDigest, Move::AgreeWrongDigest],
    );
    assert!(matches!(
        run_reason(driver, &mut endpoints).await,
        PairOutcome::ProtocolError { .. }
    ));
}

#[tokio::test]
async fn the_round_cap_stops_a_pair_that_never_agrees() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(
        a,
        b,
        "task",
        PairBounds {
            max_rounds: 2,
            ..bounds()
        },
    )
    .unwrap();
    // Neither peer ever agrees: they revise forever, so only the cap can stop
    // them. Two rounds = four scheduled turns (A, B, A, B).
    let mut endpoints = scripts(
        a,
        &[
            Move::Propose("d".to_string()),
            Move::Revise("d2".to_string()),
        ],
        b,
        &[
            Move::Revise("e".to_string()),
            Move::Revise("e2".to_string()),
        ],
    );
    assert_eq!(
        run_reason(driver, &mut endpoints).await,
        PairOutcome::CapReached { rounds: 2 }
    );
}

#[tokio::test]
async fn the_repair_drive_uses_a_repair_prompt_not_the_normal_one() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(a, b, "task", bounds()).unwrap();
    let mut endpoints = scripts(
        a,
        &[Move::Propose("d".to_string()), Move::AgreeLatest],
        b,
        &[Move::Malformed, Move::AgreeLatest],
    );
    run_reason(driver, &mut endpoints).await;

    let b_prompts: Vec<&str> = endpoints
        .log
        .iter()
        .filter_map(|call| match call {
            Call::Drive { peer, prompt } if *peer == b => Some(prompt.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(b_prompts.len(), 2, "B was driven twice: {b_prompts:?}");
    assert_eq!(
        b_prompts[0], PAIR_TURN_PROMPT,
        "the first drive is the normal turn"
    );
    assert_ne!(
        b_prompts[1], PAIR_TURN_PROMPT,
        "the repair drive must not reuse the normal prompt"
    );
    assert!(
        b_prompts[1].contains("could not be used") && b_prompts[1].contains("exactly one"),
        "the repair prompt explains the retry: {}",
        b_prompts[1]
    );
}

#[test]
fn a_zero_round_cap_is_refused_at_construction() {
    let a = SessionId::new();
    let b = SessionId::new();
    assert_eq!(
        PairDriver::new(
            a,
            b,
            "task",
            PairBounds {
                max_rounds: 0,
                ..bounds()
            }
        )
        .unwrap_err(),
        PairSetupError::ZeroRoundCap
    );
}

#[test]
fn a_driver_needs_two_distinct_peers() {
    let session = SessionId::new();
    assert_eq!(
        PairDriver::new(session, session, "task", bounds()).unwrap_err(),
        PairSetupError::DuplicatePeers
    );
}

#[tokio::test]
async fn the_run_future_is_send() {
    // Compile-time proof that a spawned/multiplexed caller can drive the pair:
    // if `run`'s future were not `Send`, this would not type-check.
    fn require_send<T: Send>(value: T) -> T {
        value
    }
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(a, b, "task", bounds()).unwrap();
    let mut endpoints = scripts(
        a,
        &[Move::Propose("d".to_string()), Move::AgreeLatest],
        b,
        &[Move::AgreeLatest],
    );
    let report = require_send(driver.run(&mut endpoints)).await;
    assert_eq!(report.reason(), &PairOutcome::Converged { revision: 1 });
}

// ── Bounds and terminal outcomes ──────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn a_slot_that_never_replies_times_out() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(
        a,
        b,
        "task",
        PairBounds {
            slot_timeout: Duration::from_millis(50),
            ..bounds()
        },
    )
    .unwrap();
    // A parks forever; paused time advances to the deadline, which cancels the
    // drive through the token it observes (never a dropped future).
    let mut endpoints = scripts(a, &[Move::Hang], b, &[]);
    assert_eq!(
        run_reason(driver, &mut endpoints).await,
        PairOutcome::TimedOut
    );
}

#[tokio::test]
async fn an_abort_stops_a_running_pair() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(a, b, "task", bounds()).unwrap();
    let abort = driver.abort_handle();
    let mut endpoints = scripts(a, &[Move::Hang], b, &[]);
    // The drive is actively parked when the abort fires; the token wakes it and
    // the run terminates.
    let report = tokio::select! {
        report = driver.run(&mut endpoints) => report,
        () = async { abort.abort(); std::future::pending::<()>().await } => unreachable!(),
    };
    assert_eq!(report.reason(), &PairOutcome::Aborted);
}

#[tokio::test]
async fn a_failed_peer_session_ends_the_pair() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(a, b, "task", bounds()).unwrap();
    let mut endpoints = scripts(a, &[Move::PeerFail], b, &[]);
    assert!(matches!(
        run_reason(driver, &mut endpoints).await,
        PairOutcome::PeerFailed { .. }
    ));
}

#[tokio::test]
async fn a_provider_error_ends_the_pair() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(a, b, "task", bounds()).unwrap();
    let mut endpoints = scripts(a, &[Move::ProviderFail], b, &[]);
    assert!(matches!(
        run_reason(driver, &mut endpoints).await,
        PairOutcome::ProviderError { .. }
    ));
}

#[tokio::test]
async fn a_delivery_that_reaches_nobody_is_not_success() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(a, b, "task", bounds()).unwrap();
    // A proposes (first turn, no delivery); B's turn needs a delivery, which
    // fails — the run reports the failure rather than pretending it landed.
    let mut endpoints = scripts(
        a,
        &[Move::Propose("x".to_string())],
        b,
        &[Move::AgreeLatest],
    )
    .with_notify(NotifyBehavior::Fail(EndpointError::PeerFailed(
        "reached nobody".to_string(),
    )));
    assert!(matches!(
        run_reason(driver, &mut endpoints).await,
        PairOutcome::PeerFailed { .. }
    ));
}

#[tokio::test(start_paused = true)]
async fn a_hung_delivery_times_out_rather_than_blocking() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(
        a,
        b,
        "task",
        PairBounds {
            slot_timeout: Duration::from_millis(50),
            ..bounds()
        },
    )
    .unwrap();
    // A proposes (slot 0, no delivery); B's slot delivers, and the delivery hangs
    // — the shared deadline cancels it and the run times out.
    let mut endpoints = scripts(
        a,
        &[Move::Propose("x".to_string())],
        b,
        &[Move::AgreeLatest],
    )
    .with_notify(NotifyBehavior::Hang);
    assert_eq!(
        run_reason(driver, &mut endpoints).await,
        PairOutcome::TimedOut
    );
}

#[tokio::test]
async fn a_pre_aborted_pair_makes_no_endpoint_calls() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(a, b, "task", bounds()).unwrap();
    let abort = driver.abort_handle();
    abort.abort();
    let mut endpoints = scripts(a, &[Move::Propose("x".to_string())], b, &[]);
    assert_eq!(
        run_reason(driver, &mut endpoints).await,
        PairOutcome::Aborted
    );
    assert!(
        endpoints.log.is_empty(),
        "a pair aborted before its turn makes zero calls: {:?}",
        endpoints.log
    );
}

#[tokio::test]
async fn spending_past_the_slot_token_budget_stops_the_pair() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(
        a,
        b,
        "task",
        PairBounds {
            slot_token_budget: 10,
            ..bounds()
        },
    )
    .unwrap();
    let mut endpoints = scripts(a, &[Move::Costly(100)], b, &[]);
    assert_eq!(
        run_reason(driver, &mut endpoints).await,
        PairOutcome::BudgetExceeded
    );
}

#[tokio::test]
async fn a_repair_does_not_reset_the_slot_budget() {
    let a = SessionId::new();
    let b = SessionId::new();
    // Budget 1: the primary (cost 1) fits, but the repair (cost 1) pushes the
    // slot's running total to 2 — over budget only if the repair shares it.
    let driver = PairDriver::new(
        a,
        b,
        "task",
        PairBounds {
            slot_token_budget: 1,
            ..bounds()
        },
    )
    .unwrap();
    let mut endpoints = scripts(
        a,
        &[Move::Propose("x".to_string())],
        b,
        &[Move::Malformed, Move::AgreeLatest],
    );
    assert_eq!(
        run_reason(driver, &mut endpoints).await,
        PairOutcome::BudgetExceeded
    );
}

#[tokio::test]
async fn a_slot_cost_that_overflows_u64_is_a_spent_budget() {
    let a = SessionId::new();
    let b = SessionId::new();
    // Budget effectively unlimited; the failure is the checked_add overflow, not
    // the threshold — the sum must never silently wrap to a small number.
    let driver = PairDriver::new(
        a,
        b,
        "task",
        PairBounds {
            slot_token_budget: u64::MAX,
            ..bounds()
        },
    )
    .unwrap();
    let mut endpoints = scripts(a, &[Move::CostlyBad(u64::MAX), Move::Costly(1)], b, &[]);
    assert_eq!(
        run_reason(driver, &mut endpoints).await,
        PairOutcome::BudgetExceeded
    );
}

#[tokio::test(start_paused = true)]
async fn a_slow_primary_does_not_grant_the_repair_a_fresh_timeout() {
    let a = SessionId::new();
    let b = SessionId::new();
    // The primary consumes 60ms of a 100ms slot; the repair alone would fit in
    // 100ms, but it shares the deadline and only 40ms remain, so it times out.
    let driver = PairDriver::new(
        a,
        b,
        "task",
        PairBounds {
            slot_timeout: Duration::from_millis(100),
            ..bounds()
        },
    )
    .unwrap();
    let mut endpoints = scripts(a, &[Move::SlowBad(60), Move::Hang], b, &[]);
    assert_eq!(
        run_reason(driver, &mut endpoints).await,
        PairOutcome::TimedOut
    );
}

#[tokio::test]
async fn an_endpoint_cancelling_without_a_signal_is_a_failed_peer() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(a, b, "task", bounds()).unwrap();
    // The endpoint reports Cancelled though the driver set neither an abort nor a
    // deadline — an anomaly, mapped to a failed peer, never a fake timeout.
    let mut endpoints = scripts(a, &[Move::SpuriousCancel], b, &[]);
    assert!(matches!(
        run_reason(driver, &mut endpoints).await,
        PairOutcome::PeerFailed { .. }
    ));
}

#[tokio::test]
async fn a_session_timeout_stop_maps_to_timed_out() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(a, b, "task", bounds()).unwrap();
    let mut endpoints = scripts(a, &[Move::SessionTimedOut], b, &[]);
    assert_eq!(
        run_reason(driver, &mut endpoints).await,
        PairOutcome::TimedOut
    );
}

#[tokio::test]
async fn a_session_budget_stop_maps_to_budget_exceeded() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(a, b, "task", bounds()).unwrap();
    let mut endpoints = scripts(a, &[Move::SessionBudget], b, &[]);
    assert_eq!(
        run_reason(driver, &mut endpoints).await,
        PairOutcome::BudgetExceeded
    );
}

#[tokio::test]
async fn a_session_no_progress_stop_maps_to_no_progress() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(a, b, "task", bounds()).unwrap();
    let mut endpoints = scripts(a, &[Move::SessionNoProgress], b, &[]);
    assert_eq!(
        run_reason(driver, &mut endpoints).await,
        PairOutcome::NoProgress
    );
}

#[tokio::test(start_paused = true)]
async fn a_result_ready_exactly_at_the_deadline_times_out() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(
        a,
        b,
        "task",
        PairBounds {
            slot_timeout: Duration::from_millis(100),
            ..bounds()
        },
    )
    .unwrap();
    // The drive result and the slot deadline both come ready at t=100ms; the
    // biased, deadline-first select must let the deadline win — a boundary hit
    // is a timeout, not an accepted result — yet the envelope is still retained.
    let mut endpoints = scripts(a, &[Move::SlowProduced(100)], b, &[]);
    let report = driver.run(&mut endpoints).await;
    assert_eq!(report.reason(), &PairOutcome::TimedOut);
    assert!(
        report.raw_for(a).is_some_and(|raw| raw.contains("late")),
        "a boundary Produced is retained even though it timed out: {:?}",
        report.raw_for(a)
    );
}

#[tokio::test(start_paused = true)]
async fn a_result_after_the_deadline_still_times_out() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(
        a,
        b,
        "task",
        PairBounds {
            slot_timeout: Duration::from_millis(100),
            ..bounds()
        },
    )
    .unwrap();
    // A cancellation-oblivious endpoint whose result lands *after* the deadline
    // fired must still be a timeout, never an accepted late result — and the late
    // envelope is retained for diagnostics.
    let mut endpoints = scripts(a, &[Move::SlowProduced(150)], b, &[]);
    let report = driver.run(&mut endpoints).await;
    assert_eq!(report.reason(), &PairOutcome::TimedOut);
    assert!(
        report.raw_for(a).is_some_and(|raw| raw.contains("late")),
        "a late Produced is retained even though it timed out: {:?}",
        report.raw_for(a)
    );
}

#[tokio::test]
async fn an_abort_during_a_hung_delivery_aborts_and_never_drives() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(a, b, "task", bounds()).unwrap();
    let abort = driver.abort_handle();
    // A proposes (slot 0); B's slot delivers, and the delivery hangs until the
    // abort fires — the pair aborts and B is never driven.
    let mut endpoints = scripts(
        a,
        &[Move::Propose("x".to_string())],
        b,
        &[Move::AgreeLatest],
    )
    .with_notify(NotifyBehavior::Hang);
    let report = tokio::select! {
        report = driver.run(&mut endpoints) => report,
        () = async { abort.abort(); std::future::pending::<()>().await } => unreachable!(),
    };
    assert_eq!(report.reason(), &PairOutcome::Aborted);
    assert_eq!(
        endpoints.drives(b),
        0,
        "B is never driven when its delivery is aborted: {:?}",
        endpoints.log
    );
}

// ── Retention and progress (the report + the observer) ────────────────────

#[tokio::test]
async fn a_converged_report_retains_peers_raw_and_candidate() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(a, b, "task", bounds()).unwrap();
    let mut endpoints = scripts(
        a,
        &[Move::Propose("agreed text".to_string()), Move::AgreeLatest],
        b,
        &[Move::AgreeLatest],
    );
    let report = driver.run(&mut endpoints).await;

    assert_eq!(report.reason(), &PairOutcome::Converged { revision: 1 });
    assert_eq!(report.peers(), [a, b]);
    // Every peer produced at least its agreement; A also proposed.
    assert!(report.raw_for(a).is_some());
    assert!(report.raw_for(b).is_some());
    let candidate = report
        .candidate()
        .expect("a converged pair has a candidate");
    assert_eq!(candidate.revision(), 1);
    assert_eq!(candidate.artifact(), "agreed text");
    assert_eq!(candidate.digest(), digest_of("agreed text"));
}

#[tokio::test]
async fn a_pre_first_response_exit_retains_nothing_invented() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(a, b, "task", bounds()).unwrap();
    // A's very first turn fails: no envelope was ever produced, none applied.
    let mut endpoints = scripts(a, &[Move::PeerFail], b, &[]);
    let report = driver.run(&mut endpoints).await;

    assert!(matches!(report.reason(), PairOutcome::PeerFailed { .. }));
    assert_eq!(report.raw(), &[None, None]);
    assert!(report.candidate().is_none());
}

#[tokio::test]
async fn a_malformed_reply_is_retained_raw_and_a_repair_replaces_it() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(a, b, "task", bounds()).unwrap();
    // A's primary is malformed (retained raw), then its repair proposes; B then
    // fails so the run ends before A produces anything else, leaving A's raw as
    // the repair's proposal.
    let mut endpoints = scripts(
        a,
        &[Move::Malformed, Move::Propose("fixed".to_string())],
        b,
        &[Move::PeerFail],
    );
    let report = driver.run(&mut endpoints).await;

    // A's retained raw is the *repair's* proposal, not the malformed primary.
    let raw_a = report.raw_for(a).expect("A produced something");
    assert!(
        raw_a.contains("propose") && raw_a.contains("fixed"),
        "the repair replaced the malformed raw: {raw_a}"
    );
    assert!(!raw_a.contains("not an envelope"));
}

#[tokio::test]
async fn an_over_budget_reply_still_retains_its_raw() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(
        a,
        b,
        "task",
        PairBounds {
            slot_token_budget: 10,
            ..bounds()
        },
    )
    .unwrap();
    // A's reply blows the budget, but the raw was recorded before the check.
    let mut endpoints = scripts(a, &[Move::Costly(100)], b, &[]);
    let report = driver.run(&mut endpoints).await;

    assert_eq!(report.reason(), &PairOutcome::BudgetExceeded);
    assert!(
        report
            .raw_for(a)
            .is_some_and(|raw| raw.contains("expensive")),
        "the over-budget raw is retained: {:?}",
        report.raw_for(a)
    );
}

#[tokio::test(start_paused = true)]
async fn a_candidate_survives_a_later_timeout() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(
        a,
        b,
        "task",
        PairBounds {
            slot_timeout: Duration::from_millis(50),
            ..bounds()
        },
    )
    .unwrap();
    // A proposes (a candidate installs); B's delivery then hangs → timeout, but
    // the candidate is still carried.
    let mut endpoints = scripts(
        a,
        &[Move::Propose("kept".to_string())],
        b,
        &[Move::AgreeLatest],
    )
    .with_notify(NotifyBehavior::Hang);
    let report = driver.run(&mut endpoints).await;

    assert_eq!(report.reason(), &PairOutcome::TimedOut);
    let candidate = report.candidate().expect("the applied candidate survives");
    assert_eq!(candidate.revision(), 1);
    assert_eq!(candidate.artifact(), "kept");
}

#[tokio::test]
async fn progress_clears_agreements_on_a_revise() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(a, b, "task", bounds()).unwrap();
    let progress = driver.progress();
    // A proposes rev1, B agrees rev1, A revises to rev2 (clears agreements),
    // then B fails — the last published progress reflects the cleared revise.
    let mut endpoints = scripts(
        a,
        &[
            Move::Propose("v1".to_string()),
            Move::Revise("v2".to_string()),
        ],
        b,
        &[Move::AgreeLatest, Move::PeerFail],
    );
    let report = driver.run(&mut endpoints).await;
    assert!(matches!(report.reason(), PairOutcome::PeerFailed { .. }));

    let latest = progress.latest();
    let candidate = latest.candidate().expect("rev2 is installed");
    assert_eq!(candidate.revision(), 2);
    // The revise wiped B's agreement: neither peer is marked agreed.
    for (_, agreed) in latest.agreements() {
        assert!(
            !agreed,
            "a revise clears every agreement: {:?}",
            latest.agreements()
        );
    }
}

#[tokio::test]
async fn progress_marks_and_clears_the_repairing_peer() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(
        a,
        b,
        "task",
        PairBounds {
            slot_timeout: Duration::from_millis(50),
            ..bounds()
        },
    )
    .unwrap();
    let mut progress = driver.progress();
    // A proposes rev1; B's primary is malformed, so its one repair is spent on the
    // same peer. The repair drive then parks until the slot deadline, holding the
    // repair snapshot long enough for a concurrent observer to see it.
    let mut endpoints = scripts(
        a,
        &[Move::Propose("v1".to_string())],
        b,
        &[Move::Malformed, Move::Hang],
    );

    let mut seen = Vec::new();
    let run = driver.run(&mut endpoints);
    tokio::pin!(run);
    let report = loop {
        tokio::select! {
            outcome = &mut run => break outcome,
            Some(snapshot) = progress.changed() => seen.push(snapshot),
        }
    };
    // The repair timed out (its drive never produced), so the terminal reason is a
    // timeout — the point under test is the transient repair signal, not the reason.
    assert_eq!(report.reason(), &PairOutcome::TimedOut);

    let repairing = seen
        .iter()
        .find(|snapshot| snapshot.repairing_peer().is_some())
        .expect("a repair snapshot was published while B repaired");
    assert_eq!(repairing.repairing_peer(), Some(b));
    // The same peer stays scheduled for its repair drive, and the installed candidate
    // is unchanged by the repair decision.
    assert_eq!(repairing.next_peer(), Some(b));
    assert_eq!(
        repairing.candidate().map(CandidateSnapshot::revision),
        Some(1)
    );

    // The terminal snapshot clears the repair signal: the authoritative latest value
    // after the run has finished carries no repairing peer.
    assert_eq!(progress.latest().repairing_peer(), None);
}

#[tokio::test]
async fn progress_clears_the_repair_signal_on_the_next_valid_slot() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(
        a,
        b,
        "task",
        PairBounds {
            slot_timeout: Duration::from_millis(50),
            ..bounds()
        },
    )
    .unwrap();
    // A proposes rev1; B's primary is malformed and its one repair agrees; A's next
    // turn then hangs, holding the post-repair scheduled snapshot long enough for a
    // concurrent observer to read the cleared repair signal off the watch.
    let mut endpoints = scripts(
        a,
        &[Move::Propose("v1".to_string()), Move::Hang],
        b,
        &[Move::Malformed, Move::AgreeLatest],
    );

    let mut progress = driver.progress();
    let run = driver.run(&mut endpoints);
    tokio::pin!(run);
    let mut seen = Vec::new();
    let report = loop {
        tokio::select! {
            outcome = &mut run => break outcome,
            Some(snapshot) = progress.changed() => seen.push(snapshot),
        }
    };

    // After the valid repair, the next scheduled (non-terminal) snapshot carries no
    // repair signal, the applied candidate, and B's fresh agreement — the clear is
    // authoritative, not merely the terminal value.
    let cleared = seen
        .iter()
        .find(|snapshot| snapshot.next_peer() == Some(a) && snapshot.candidate().is_some())
        .expect("a post-repair scheduled snapshot");
    assert_eq!(cleared.repairing_peer(), None);
    assert_eq!(
        cleared.candidate().map(CandidateSnapshot::revision),
        Some(1)
    );
    assert!(
        cleared
            .agreements()
            .iter()
            .any(|(id, agreed)| *id == b && *agreed),
        "B's repair agreement is reflected before A's turn"
    );

    assert_eq!(report.reason(), &PairOutcome::TimedOut);
    assert_eq!(progress.latest().repairing_peer(), None);
}

#[tokio::test]
async fn progress_advances_agreements_one_then_two() {
    let a = SessionId::new();
    let b = SessionId::new();

    // One agreement: B agrees rev1, then A fails — exactly one peer is agreed.
    let driver = PairDriver::new(a, b, "task", bounds()).unwrap();
    let progress = driver.progress();
    let mut endpoints = scripts(
        a,
        &[Move::Propose("v1".to_string()), Move::PeerFail],
        b,
        &[Move::AgreeLatest],
    );
    let _ = driver.run(&mut endpoints).await;
    let one = progress.latest();
    let agreed_count = one
        .agreements()
        .iter()
        .filter(|(_, agreed)| *agreed)
        .count();
    assert_eq!(agreed_count, 1, "one peer agreed: {:?}", one.agreements());
    assert!(
        one.agreements()
            .iter()
            .any(|(peer, agreed)| *peer == b && *agreed),
        "the agreed peer is B: {:?}",
        one.agreements()
    );

    // Two agreements: a full convergence marks both.
    let driver = PairDriver::new(a, b, "task", bounds()).unwrap();
    let progress = driver.progress();
    let mut endpoints = scripts(
        a,
        &[Move::Propose("v1".to_string()), Move::AgreeLatest],
        b,
        &[Move::AgreeLatest],
    );
    let report = driver.run(&mut endpoints).await;
    assert_eq!(report.reason(), &PairOutcome::Converged { revision: 1 });
    let two = progress.latest();
    assert!(
        two.agreements().iter().all(|(_, agreed)| *agreed),
        "both peers agreed at convergence: {:?}",
        two.agreements()
    );
    assert_eq!(two.max_rounds(), bounds().max_rounds);
}

#[tokio::test]
async fn an_abort_with_a_produced_reply_aborts_but_retains_the_raw() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(a, b, "task", bounds()).unwrap();
    let abort = driver.abort_handle();
    // A's drive aborts the pair from inside, then still produces: the reason is
    // Aborted (precedence wins), but the produced raw was recorded first.
    let mut endpoints = scripts(a, &[Move::AbortThenProduce(abort)], b, &[]);
    let report = driver.run(&mut endpoints).await;
    assert_eq!(report.reason(), &PairOutcome::Aborted);
    assert!(
        report.raw_for(a).is_some_and(|raw| raw.contains("late")),
        "the produced raw is retained despite the abort: {:?}",
        report.raw_for(a)
    );
}

#[tokio::test]
async fn an_abort_during_an_observed_repair_ends_aborted_and_never_reaches_the_other_peer() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(
        a,
        b,
        "task",
        PairBounds {
            slot_timeout: Duration::from_secs(5),
            ..bounds()
        },
    )
    .unwrap();
    let abort = driver.abort_handle();
    // A's primary is malformed, so its one repair drive runs; that repair hangs, so the
    // authoritative repairing snapshot is observable. We abort at exactly that seam —
    // during the observed repair — rather than inferring it from call counts.
    let mut endpoints = scripts(a, &[Move::Malformed, Move::Hang], b, &[]);
    let mut progress = driver.progress();
    let mut observed_repair = false;
    // Scope the run future so its `&mut endpoints` borrow is released before the
    // drive-count assertions below.
    let report = {
        let run = driver.run(&mut endpoints);
        tokio::pin!(run);
        loop {
            tokio::select! {
                outcome = &mut run => break outcome,
                Some(snapshot) = progress.changed() => {
                    if snapshot.repairing_peer() == Some(a) {
                        assert_eq!(snapshot.next_peer(), Some(a));
                        observed_repair = true;
                        abort.abort();
                    }
                }
            }
        }
    };

    assert!(
        observed_repair,
        "the transient repairing snapshot was observed before the abort"
    );
    assert_eq!(report.reason(), &PairOutcome::Aborted);
    // A was driven twice — the malformed primary and the aborting repair; B is never
    // scheduled or driven.
    assert_eq!(endpoints.drives(a), 2, "A was re-driven for the repair");
    assert_eq!(endpoints.drives(b), 0, "B is never driven");
    assert!(
        report.raw_for(a).is_some(),
        "the malformed raw is retained through the abort"
    );
    assert_eq!(
        progress.latest().repairing_peer(),
        None,
        "the repair signal clears terminally"
    );
}

#[tokio::test]
async fn progress_can_be_observed_live_then_closes() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(
        a,
        b,
        "task",
        PairBounds {
            max_rounds: 1,
            ..bounds()
        },
    )
    .unwrap();
    let mut rx = driver.progress();
    // A proposes instantly (a publish), then B revises slowly so the run yields
    // and the observer sees a live update before the run finishes.
    let mut endpoints = scripts(
        a,
        &[Move::Propose("v1".to_string())],
        b,
        &[Move::SlowRevise(10)],
    );
    let observer = async {
        let mut seen = 0usize;
        while rx.changed().await.is_some() {
            seen += 1;
        }
        seen
    };
    let (report, seen) = tokio::join!(driver.run(&mut endpoints), observer);
    assert_eq!(report.reason(), &PairOutcome::CapReached { rounds: 1 });
    assert!(seen >= 1, "at least one live progress update was observed");
    // The `while` loop exited, which only happens when `changed()` returned
    // `None` — i.e. the sole sender dropped as `run` finished.
}

#[tokio::test]
async fn next_peer_and_rounds_are_truthful_at_boundaries() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(
        a,
        b,
        "task",
        PairBounds {
            max_rounds: 1,
            ..bounds()
        },
    )
    .unwrap();
    let mut rx = driver.progress();

    // The initial seed points at the first peer with no rounds completed.
    let seed = rx.latest();
    assert_eq!(seed.next_peer(), Some(a));
    assert_eq!(seed.completed_rounds(), 0);

    // A proposes, then B revises slowly (so the run yields); max_rounds=1 ends it
    // in CapReached after B's slot.
    let mut endpoints = scripts(
        a,
        &[Move::Propose("v1".to_string())],
        b,
        &[Move::SlowRevise(10)],
    );
    let collector = async {
        let mut seq = Vec::new();
        while let Some(update) = rx.changed().await {
            seq.push(update);
        }
        seq
    };
    let (report, seq) = tokio::join!(driver.run(&mut endpoints), collector);
    assert_eq!(report.reason(), &PairOutcome::CapReached { rounds: 1 });

    // After the first nonterminal slot the next peer flips to B.
    assert!(
        seq.iter().any(|update| update.next_peer() == Some(b)),
        "next_peer flips to B: {:?}",
        seq.iter().map(PairProgress::next_peer).collect::<Vec<_>>()
    );
    // The final snapshot claims no next peer and the incremented round count.
    let last = seq.last().expect("a final publish");
    assert_eq!(last.next_peer(), None);
    assert_eq!(last.completed_rounds(), 1);
}

#[tokio::test]
async fn converging_on_the_second_peers_slot_counts_the_completed_round() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(
        a,
        b,
        "task",
        PairBounds {
            max_rounds: 5,
            ..bounds()
        },
    )
    .unwrap();
    let rx = driver.progress();
    // A propose v1; B revise v2 (round 1 completes); A agree v2; B agree v2 →
    // Converged on B's (second-peer) slot, which completes round 2.
    let mut endpoints = scripts(
        a,
        &[Move::Propose("v1".to_string()), Move::AgreeLatest],
        b,
        &[Move::Revise("v2".to_string()), Move::AgreeLatest],
    );
    let report = driver.run(&mut endpoints).await;
    assert_eq!(report.reason(), &PairOutcome::Converged { revision: 2 });

    let last = rx.latest();
    assert_eq!(last.next_peer(), None);
    assert_eq!(
        last.completed_rounds(),
        2,
        "the second-peer converging slot completes round 2"
    );
    assert!(
        last.agreements().iter().all(|(_, agreed)| *agreed),
        "both peers agreed: {:?}",
        last.agreements()
    );
}

// ── Retention matrix: all nine reasons keep peers and honest options ──────

/// Run and assert the report always carries the two stable peers.
async fn report_of(
    a: SessionId,
    b: SessionId,
    bounds: PairBounds,
    endpoints: &mut FakeEndpoints,
) -> PairReport {
    let driver = PairDriver::new(a, b, "task", bounds).unwrap();
    let report = driver.run(endpoints).await;
    assert_eq!(
        report.peers(),
        [a, b],
        "peers are always the two scheduled ones"
    );
    report
}

#[tokio::test]
async fn every_reason_keeps_the_two_peers_and_honest_options() {
    let a = SessionId::new();
    let b = SessionId::new();

    // Converged: both raw present, candidate present.
    let mut ep = scripts(
        a,
        &[Move::Propose("x".to_string()), Move::AgreeLatest],
        b,
        &[Move::AgreeLatest],
    );
    let report = report_of(a, b, bounds(), &mut ep).await;
    assert!(matches!(report.reason(), PairOutcome::Converged { .. }));
    assert!(report.candidate().is_some() && report.raw_for(a).is_some());

    // ProtocolError before any candidate: A's two malformeds; no candidate.
    let mut ep = scripts(a, &[Move::Malformed, Move::Malformed], b, &[]);
    let report = report_of(a, b, bounds(), &mut ep).await;
    assert!(matches!(report.reason(), PairOutcome::ProtocolError { .. }));
    assert!(report.candidate().is_none() && report.raw_for(a).is_some());

    // CapReached: a candidate exists, no convergence.
    let mut ep = scripts(
        a,
        &[Move::Propose("x".to_string())],
        b,
        &[Move::Revise("y".to_string())],
    );
    let report = report_of(
        a,
        b,
        PairBounds {
            max_rounds: 1,
            ..bounds()
        },
        &mut ep,
    )
    .await;
    assert!(matches!(report.reason(), PairOutcome::CapReached { .. }));
    assert!(report.candidate().is_some());

    // PeerFailed pre-candidate: nothing invented.
    let mut ep = scripts(a, &[Move::PeerFail], b, &[]);
    let report = report_of(a, b, bounds(), &mut ep).await;
    assert!(matches!(report.reason(), PairOutcome::PeerFailed { .. }));
    assert_eq!(report.raw(), &[None, None]);
    assert!(report.candidate().is_none());

    // ProviderError: reason present.
    let mut ep = scripts(a, &[Move::ProviderFail], b, &[]);
    let report = report_of(a, b, bounds(), &mut ep).await;
    assert!(matches!(report.reason(), PairOutcome::ProviderError { .. }));

    // BudgetExceeded: raw retained.
    let mut ep = scripts(a, &[Move::Costly(100)], b, &[]);
    let report = report_of(
        a,
        b,
        PairBounds {
            slot_token_budget: 10,
            ..bounds()
        },
        &mut ep,
    )
    .await;
    assert!(matches!(report.reason(), PairOutcome::BudgetExceeded));
    assert!(report.raw_for(a).is_some());

    // NoProgress: session stop, no candidate produced.
    let mut ep = scripts(a, &[Move::SessionNoProgress], b, &[]);
    let report = report_of(a, b, bounds(), &mut ep).await;
    assert!(matches!(report.reason(), PairOutcome::NoProgress));
    assert!(report.candidate().is_none());
}

#[tokio::test]
async fn a_candidate_survives_a_later_peer_failure() {
    let a = SessionId::new();
    let b = SessionId::new();
    let mut ep = scripts(
        a,
        &[Move::Propose("kept".to_string())],
        b,
        &[Move::PeerFail],
    );
    let report = report_of(a, b, bounds(), &mut ep).await;

    assert!(matches!(report.reason(), PairOutcome::PeerFailed { .. }));
    let candidate = report
        .candidate()
        .expect("the applied candidate survives a peer failure");
    assert_eq!(candidate.revision(), 1);
    assert_eq!(candidate.artifact(), "kept");
}

#[tokio::test]
async fn a_candidate_survives_a_later_abort() {
    let a = SessionId::new();
    let b = SessionId::new();
    let driver = PairDriver::new(a, b, "task", bounds()).unwrap();
    let abort = driver.abort_handle();
    // A proposes (a candidate installs, applied); then B's delivery aborts the
    // pair — the candidate is still carried into the report.
    let mut endpoints = scripts(
        a,
        &[Move::Propose("kept".to_string())],
        b,
        &[Move::AgreeLatest],
    )
    .with_notify(NotifyBehavior::AbortThenHang(abort));
    let report = driver.run(&mut endpoints).await;
    assert_eq!(report.reason(), &PairOutcome::Aborted);
    let candidate = report
        .candidate()
        .expect("the applied candidate survives an abort");
    assert_eq!(candidate.revision(), 1);
    assert_eq!(candidate.artifact(), "kept");
}
