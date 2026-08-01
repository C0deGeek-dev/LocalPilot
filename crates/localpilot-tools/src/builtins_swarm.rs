//! `swarm` — talk to the other agents working on this repository.
//!
//! One tool with several actions rather than several tools, for a reason that is
//! about models rather than tidiness: a session that is *not* in a swarm should
//! cost one unused tool in the schema, not four. And a model that has just been
//! told about `swarm_send`, `swarm_broadcast`, and `swarm_roster` will reliably
//! invent `swarm_reply`.
//!
//! Which is the other half of the design. Models do not call an action verb the
//! way the schema spells it — they write `send`, `dm`, `tell`, `msg`, `announce`,
//! and put the body under `text` or `content` or `message`. Rejecting those is a
//! wasted turn every time, and the model usually gets it wrong the same way
//! again. So the verbs and the field names are both normalised before validation,
//! and only a genuinely unreadable call is refused.
//!
//! The tool declares **no effects**: sending a message touches no file and runs
//! no command. The gate is the host capability — a session with no peers cannot
//! message anyone however permissive its profile, and one with peers is not
//! stopped by a restrictive one.

use async_trait::async_trait;
use localpilot_sandbox::Effect;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::error::ToolError;
use crate::tool::{
    parse_input, schema_for, Audience, Delivery, PeerMessage, Tool, ToolContext, ToolOutput,
};

/// The model-callable name.
pub const SWARM: &str = "swarm";

/// Above this many bytes, a body needs a one-line summary.
///
/// The recipient is another agent mid-task. It has to decide whether this is
/// worth breaking off for, and it cannot make that decision by reading the whole
/// thing — that *is* breaking off for it.
const TLDR_REQUIRED_ABOVE: usize = 600;

/// What the model is told when this session is not collaborating. A
/// model-visible string rather than an error: "there is nobody to talk to" is
/// information, not a failure.
const UNAVAILABLE: &str =
    "swarm is not available in this session: it is not part of a swarm, so there is nobody to \
     message. Do the work directly.";

#[derive(Debug, Deserialize, JsonSchema)]
struct SwarmInput {
    /// What to do: `send` a message to one peer, `broadcast` to the agents you
    /// spawned (or, if you are the coordinator, to the whole swarm), or
    /// `roster` to see who is here.
    action: String,
    /// For `send`: the peer, by name or session id.
    #[serde(default, alias = "recipient", alias = "target", alias = "peer")]
    to: Option<String>,
    /// A one-line summary. Required when the body is long.
    #[serde(default, alias = "summary", alias = "subject")]
    tldr: Option<String>,
    /// The message itself.
    #[serde(default, alias = "text", alias = "content", alias = "message")]
    body: Option<String>,
    /// `notify` (default) to leave the recipient undisturbed, `interrupt` to
    /// reach its running turn, or `wake` to also start a turn if it is idle.
    #[serde(default, alias = "mode", alias = "urgency")]
    delivery: Option<String>,
    /// For `broadcast`: `swarm` to address every member. Coordinator only;
    /// otherwise the message goes to the agents you spawned.
    #[serde(default)]
    scope: Option<String>,
}

/// What the caller asked for, once the synonyms are resolved.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    Send,
    Broadcast,
    Roster,
}

/// Message the agents working alongside you.
pub struct Swarm;

#[async_trait]
impl Tool for Swarm {
    fn name(&self) -> &str {
        SWARM
    }

    fn description(&self) -> &str {
        "Talk to the other agents working on this repository. `send` messages one peer by name or \
         id; `broadcast` reaches the agents you spawned (the whole swarm if you are the \
         coordinator); `roster` lists who is here and what they are doing. Use it to hand over a \
         finding another agent needs, to warn about a file you are changing, or to ask a peer \
         something only it knows — not to narrate progress. Long messages need a one-line `tldr`, \
         because the recipient is mid-task and has to decide whether to break off."
    }

    fn schema(&self) -> Value {
        schema_for::<SwarmInput>()
    }

    fn effects(&self, _input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        // A message touches nothing. The host capability is the gate.
        Ok(Vec::new())
    }

    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: SwarmInput = parse_input(&input)?;
        let action = read_action(&input.action)?;

        // Validate before checking the capability, so a malformed call is
        // reported as malformed even outside a swarm — otherwise the mistake
        // hides until the one run where a swarm exists.
        let request = match action {
            Action::Roster => None,
            Action::Send => Some(build_send(&input)?),
            Action::Broadcast => Some(build_broadcast(&input)?),
        };

        let Some(peers) = ctx.peers else {
            return Ok(ToolOutput::ok(UNAVAILABLE));
        };

        let Some(message) = request else {
            return Ok(ToolOutput::ok(render_roster(peers).await));
        };
        match peers.send(&message).await {
            Ok(delivered) if delivered.reached == 0 => Ok(ToolOutput::ok(
                "Nobody received that: there is no peer matching that audience right now."
                    .to_string(),
            )),
            Ok(delivered) => Ok(ToolOutput::ok(format!(
                "Delivered to {} ({}).",
                delivered.reached,
                delivered.recipients.join(", ")
            ))),
            Err(reason) => Err(ToolError::InvalidInput(reason)),
        }
    }
}

/// Resolve whatever the model wrote into one of the three actions.
///
/// The synonym lists are not politeness. A model that has to guess a verb will
/// guess a *reasonable* one, and refusing it costs a turn and usually produces
/// the same guess again.
fn read_action(raw: &str) -> Result<Action, ToolError> {
    let normalised: String = raw
        .trim()
        .to_ascii_lowercase()
        .replace(['-', ' '], "_")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    match normalised.as_str() {
        "send" | "dm" | "message" | "msg" | "tell" | "reply" | "respond" | "notify" | "ask" => {
            Ok(Action::Send)
        }
        "broadcast" | "announce" | "all" | "everyone" | "shout" | "send_all" => {
            Ok(Action::Broadcast)
        }
        "roster" | "list" | "who" | "members" | "peers" | "status" => Ok(Action::Roster),
        _ => Err(ToolError::InvalidInput(format!(
            "unknown swarm action {raw:?} — use \"send\" (one peer), \"broadcast\" (several), or \
             \"roster\" (who is here)"
        ))),
    }
}

/// Resolve the delivery mode, defaulting to the least disruptive one.
fn read_delivery(raw: Option<&str>) -> Result<Delivery, ToolError> {
    let Some(raw) = raw else {
        return Ok(Delivery::Notify);
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "notify" | "quiet" | "background" | "fyi" => Ok(Delivery::Notify),
        "interrupt" | "urgent" | "now" | "immediate" => Ok(Delivery::Interrupt),
        "wake" | "start" | "rouse" => Ok(Delivery::Wake),
        other => Err(ToolError::InvalidInput(format!(
            "unknown delivery mode {other:?} — use \"notify\", \"interrupt\", or \"wake\""
        ))),
    }
}

fn build_send(input: &SwarmInput) -> Result<PeerMessage, ToolError> {
    let to = input
        .to
        .as_deref()
        .map(str::trim)
        .filter(|to| !to.is_empty())
        .ok_or_else(|| {
            ToolError::InvalidInput(
                "`send` needs a `to`: the peer's name or session id. Call `roster` if you do not \
                 know who is here."
                    .to_string(),
            )
        })?;
    Ok(PeerMessage {
        audience: Audience::One(to.to_string()),
        tldr: read_tldr(input)?,
        body: read_body(input)?,
        delivery: read_delivery(input.delivery.as_deref())?,
    })
}

fn build_broadcast(input: &SwarmInput) -> Result<PeerMessage, ToolError> {
    let audience = match input.scope.as_deref().map(str::trim) {
        Some(scope) if scope.eq_ignore_ascii_case("swarm") || scope.eq_ignore_ascii_case("all") => {
            Audience::Swarm
        }
        _ => Audience::Subtree,
    };
    Ok(PeerMessage {
        audience,
        tldr: read_tldr(input)?,
        body: read_body(input)?,
        delivery: read_delivery(input.delivery.as_deref())?,
    })
}

fn read_body(input: &SwarmInput) -> Result<String, ToolError> {
    let body = input
        .body
        .as_deref()
        .map(str::trim)
        .filter(|body| !body.is_empty())
        .ok_or_else(|| ToolError::InvalidInput("a swarm message needs a `body`".to_string()))?;
    Ok(body.to_string())
}

/// Require a summary for a long body, and say why rather than merely refusing.
fn read_tldr(input: &SwarmInput) -> Result<Option<String>, ToolError> {
    let tldr = input
        .tldr
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(ToString::to_string);
    let body_len = input.body.as_deref().map_or(0, str::len);
    if body_len > TLDR_REQUIRED_ABOVE && tldr.is_none() {
        return Err(ToolError::InvalidInput(format!(
            "this message is {body_len} bytes, so it needs a one-line `tldr`. The agent reading it \
             is in the middle of its own task and has to decide whether to break off — it cannot \
             make that decision by reading the whole message."
        )));
    }
    Ok(tldr)
}

/// Render the roster as a short table the model can act on.
async fn render_roster(peers: &dyn crate::tool::SwarmPeers) -> String {
    let me = peers.identity().await;
    let roster = peers.roster().await;
    let mut out = format!(
        "You are {} ({}){}.\n",
        me.name,
        me.session,
        if me.is_coordinator {
            ", the coordinator"
        } else {
            ""
        }
    );
    if roster.is_empty() {
        out.push_str("\nNobody else is in this swarm yet.");
        return out;
    }
    out.push_str("\nOther members:\n");
    for peer in roster {
        out.push_str(&format!(
            "- {} ({}) — {}, {}{}\n",
            peer.name,
            peer.session,
            peer.role,
            peer.status,
            if peer.in_my_subtree { ", yours" } else { "" }
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{Delivered, PeerSummary, SwarmIdentity, SwarmPeers};
    use serde_json::json;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Peers {
        sent: Mutex<Vec<PeerMessage>>,
        refuse: Option<String>,
        roster: Vec<PeerSummary>,
        coordinator: bool,
    }

    impl SwarmPeers for Peers {
        fn identity<'a>(
            &'a self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = SwarmIdentity> + Send + 'a>>
        {
            Box::pin(async move {
                SwarmIdentity {
                    session: "me-0001".into(),
                    name: "lead".into(),
                    is_coordinator: self.coordinator,
                }
            })
        }

        fn roster<'a>(
            &'a self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<PeerSummary>> + Send + 'a>>
        {
            Box::pin(async move { self.roster.clone() })
        }

        fn send<'a>(
            &'a self,
            message: &'a PeerMessage,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Delivered, String>> + Send + 'a>,
        > {
            Box::pin(async move {
                if let Some(reason) = &self.refuse {
                    return Err(reason.clone());
                }
                self.sent
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(message.clone());
                Ok(Delivered {
                    reached: 1,
                    recipients: vec!["reviewer".into()],
                })
            })
        }
    }

    fn ctx<'a>(
        workspace: &'a localpilot_sandbox::Workspace,
        peers: Option<&'a dyn SwarmPeers>,
    ) -> ToolContext<'a> {
        ToolContext {
            workspace,
            interactivity: localpilot_sandbox::Interactivity::NonInteractive,
            trusted: true,
            retention: None,
            processes: None,
            agents: None,
            prompter: None,
            peers,
        }
    }

    fn workspace() -> (tempfile::TempDir, localpilot_sandbox::Workspace) {
        let dir = tempfile::tempdir().unwrap();
        let workspace = localpilot_sandbox::Workspace::new(dir.path()).unwrap();
        (dir, workspace)
    }

    #[test]
    fn every_reasonable_verb_for_one_recipient_resolves() {
        for verb in ["send", "dm", "message", "msg", "tell", "Reply", "SEND"] {
            assert_eq!(read_action(verb).unwrap(), Action::Send, "{verb}");
        }
        for verb in ["broadcast", "announce", "everyone", "send-all", "send all"] {
            assert_eq!(read_action(verb).unwrap(), Action::Broadcast, "{verb}");
        }
        for verb in ["roster", "list", "who", "members"] {
            assert_eq!(read_action(verb).unwrap(), Action::Roster, "{verb}");
        }
    }

    #[test]
    fn an_unreadable_verb_says_what_the_choices_are() {
        let error = read_action("teleport").unwrap_err().to_string();
        assert!(error.contains("send"), "{error}");
        assert!(error.contains("broadcast"), "{error}");
        assert!(error.contains("roster"), "{error}");
    }

    #[test]
    fn delivery_modes_and_their_synonyms_resolve() {
        assert_eq!(read_delivery(None).unwrap(), Delivery::Notify);
        assert_eq!(read_delivery(Some("fyi")).unwrap(), Delivery::Notify);
        assert_eq!(read_delivery(Some("URGENT")).unwrap(), Delivery::Interrupt);
        assert_eq!(read_delivery(Some("wake")).unwrap(), Delivery::Wake);
        assert!(read_delivery(Some("telepathy")).is_err());
    }

    #[tokio::test]
    async fn the_body_is_accepted_under_any_of_its_usual_names() {
        let (_dir, workspace) = workspace();
        for field in ["body", "text", "content", "message"] {
            let peers = Peers::default();
            let ctx = ctx(&workspace, Some(&peers));
            let input = json!({ "action": "send", "to": "reviewer", field: "look at parse.rs" });
            let out = Swarm.invoke(input, &ctx).await.unwrap();
            assert!(out.text.contains("Delivered"), "{field}: {}", out.text);
            assert_eq!(
                peers.sent.lock().unwrap()[0].body,
                "look at parse.rs",
                "{field}"
            );
        }
    }

    #[tokio::test]
    async fn a_recipient_is_accepted_under_any_of_its_usual_names() {
        let (_dir, workspace) = workspace();
        for field in ["to", "recipient", "target", "peer"] {
            let peers = Peers::default();
            let ctx = ctx(&workspace, Some(&peers));
            let input = json!({ "action": "dm", field: "reviewer", "body": "hello" });
            Swarm.invoke(input, &ctx).await.unwrap();
            assert_eq!(
                peers.sent.lock().unwrap()[0].audience,
                Audience::One("reviewer".into()),
                "{field}"
            );
        }
    }

    #[tokio::test]
    async fn a_long_body_without_a_summary_is_refused_with_the_reason() {
        let (_dir, workspace) = workspace();
        let peers = Peers::default();
        let ctx = ctx(&workspace, Some(&peers));
        let long = "x".repeat(TLDR_REQUIRED_ABOVE + 1);
        let error = Swarm
            .invoke(
                json!({ "action": "send", "to": "reviewer", "body": long }),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("tldr"), "{error}");
        assert!(error.contains("break off"), "{error}");
        assert!(peers.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_long_body_with_a_summary_goes_through() {
        let (_dir, workspace) = workspace();
        let peers = Peers::default();
        let ctx = ctx(&workspace, Some(&peers));
        let long = "x".repeat(TLDR_REQUIRED_ABOVE + 1);
        Swarm
            .invoke(
                json!({
                    "action": "send",
                    "to": "reviewer",
                    "tldr": "parse.rs needs a second look",
                    "body": long
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(
            peers.sent.lock().unwrap()[0].tldr.as_deref(),
            Some("parse.rs needs a second look")
        );
    }

    #[tokio::test]
    async fn a_broadcast_defaults_to_the_senders_own_subtree() {
        let (_dir, workspace) = workspace();
        let peers = Peers::default();
        let ctx = ctx(&workspace, Some(&peers));
        Swarm
            .invoke(
                json!({ "action": "announce", "body": "I am editing parse.rs" }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(peers.sent.lock().unwrap()[0].audience, Audience::Subtree);
    }

    #[tokio::test]
    async fn asking_for_the_whole_swarm_asks_for_the_whole_swarm() {
        let (_dir, workspace) = workspace();
        let peers = Peers::default();
        let ctx = ctx(&workspace, Some(&peers));
        Swarm
            .invoke(
                json!({ "action": "broadcast", "scope": "swarm", "body": "stop" }),
                &ctx,
            )
            .await
            .unwrap();
        // Whether it is *allowed* is the host's decision, not the tool's — the
        // tool's job is to report the ask faithfully.
        assert_eq!(peers.sent.lock().unwrap()[0].audience, Audience::Swarm);
    }

    #[tokio::test]
    async fn a_refusal_from_the_host_reaches_the_model_intact() {
        let (_dir, workspace) = workspace();
        let peers = Peers {
            refuse: Some("only the coordinator may address the whole swarm".into()),
            ..Peers::default()
        };
        let ctx = ctx(&workspace, Some(&peers));
        let error = Swarm
            .invoke(
                json!({ "action": "broadcast", "scope": "swarm", "body": "stop" }),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("only the coordinator"), "{error}");
    }

    #[tokio::test]
    async fn a_send_with_no_recipient_says_to_check_the_roster() {
        let (_dir, workspace) = workspace();
        let peers = Peers::default();
        let ctx = ctx(&workspace, Some(&peers));
        let error = Swarm
            .invoke(json!({ "action": "send", "body": "hello" }), &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("roster"), "{error}");
    }

    #[tokio::test]
    async fn the_roster_shows_who_is_here_and_who_is_mine() {
        let (_dir, workspace) = workspace();
        let peers = Peers {
            coordinator: true,
            roster: vec![
                PeerSummary {
                    session: "a-0001".into(),
                    name: "reviewer".into(),
                    role: "worker".into(),
                    status: "active".into(),
                    in_my_subtree: true,
                },
                PeerSummary {
                    session: "b-0002".into(),
                    name: "stranger".into(),
                    role: "worker".into(),
                    status: "finished".into(),
                    in_my_subtree: false,
                },
            ],
            ..Peers::default()
        };
        let ctx = ctx(&workspace, Some(&peers));
        let out = Swarm
            .invoke(json!({ "action": "who" }), &ctx)
            .await
            .unwrap();
        let text = &out.text;
        assert!(text.contains("the coordinator"), "{text}");
        assert!(text.contains("reviewer (a-0001)"), "{text}");
        assert!(text.contains(", yours"), "{text}");
        assert!(text.contains("stranger"), "{text}");
        assert!(!text.contains("stranger (b-0002) — worker, finished, yours"));
    }

    #[tokio::test]
    async fn outside_a_swarm_the_tool_says_so_rather_than_failing() {
        let (_dir, workspace) = workspace();
        let ctx = ctx(&workspace, None);
        let out = Swarm
            .invoke(
                json!({ "action": "send", "to": "reviewer", "body": "hello" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.text.contains("not part of a swarm"));
    }

    #[tokio::test]
    async fn a_malformed_call_is_still_malformed_outside_a_swarm() {
        let (_dir, workspace) = workspace();
        let ctx = ctx(&workspace, None);
        // Validated before the capability check, so this mistake cannot hide
        // until the one run where a swarm happens to exist.
        assert!(Swarm
            .invoke(json!({ "action": "send", "body": "hello" }), &ctx)
            .await
            .is_err());
    }
}
