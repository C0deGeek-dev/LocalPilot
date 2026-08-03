//! Routing a message from one member to others, and getting it into their turns.
//!
//! The tool decides *what* was asked for; this decides who that reaches and
//! whether the asker is allowed to reach them. Two rules carry the weight:
//!
//! - **Scope is the spawn tree.** A member may address what it spawned, and no
//!   more. Only the coordinator may address the whole swarm. Without that, one
//!   worker deciding to "let everyone know" costs every other worker a turn, and
//!   the cost scales with the square of the swarm.
//! - **Delivery is one substrate.** `notify`, `interrupt`, and `wake` all end up
//!   on the same soft-interrupt queue the user's own steering uses; they differ
//!   only in whether they wait for the recipient's next turn or push into the
//!   current one. A second delivery channel would be a second set of ordering
//!   rules to get wrong.

use std::sync::Arc;

use localpilot_core::SessionId;
use localpilot_harness::{SoftInterrupt, SoftInterruptSource};
use localpilot_tools::{
    Audience, Delivered, Delivery, PeerMessage, PeerSummary, SwarmIdentity, SwarmPeers,
};

use super::registry::{MemberRole, MemberStatus, SwarmMember};
use super::scope::SwarmId;
use super::spawn::SwarmHost;

/// One session's view of its swarm, handed to the `swarm` tool through the tool
/// context.
///
/// Bound to a session at construction, so the tool never has to be told — and
/// can never be wrong about — who it is speaking as.
pub struct SessionPeers {
    host: SwarmHost,
    swarm: SwarmId,
    me: SessionId,
}

impl SessionPeers {
    /// Bind a view for `session`.
    #[must_use]
    pub fn new(host: SwarmHost, swarm: SwarmId, session: SessionId) -> Self {
        Self {
            host,
            swarm,
            me: session,
        }
    }

    /// Resolve an audience to concrete recipients, refusing what this session
    /// may not address.
    async fn recipients(&self, audience: &Audience) -> Result<Vec<SwarmMember>, String> {
        let swarms = self.host.swarms();
        match audience {
            Audience::One(who) => {
                let target = match who.parse::<SessionId>() {
                    Ok(id) => id,
                    Err(_) => swarms
                        .resolve_name(&self.swarm, who)
                        .await
                        .map_err(|e| e.to_string())?,
                };
                if target == self.me {
                    return Err("that is you — messaging yourself does nothing".to_string());
                }
                swarms
                    .member(&self.swarm, target)
                    .await
                    .map(|member| vec![member])
                    .ok_or_else(|| format!("no member of this swarm is {who}"))
            }
            Audience::Subtree => {
                let ids = swarms.subtree(&self.swarm, self.me).await;
                Ok(self
                    .members(ids.into_iter().filter(|id| *id != self.me))
                    .await)
            }
            Audience::Swarm => {
                if !swarms.is_coordinator(&self.swarm, self.me).await {
                    return Err(
                        "only the coordinator may address the whole swarm. Broadcast to the \
                         agents you spawned instead, or ask the coordinator to relay it."
                            .to_string(),
                    );
                }
                let ids = swarms
                    .members(&self.swarm)
                    .await
                    .into_iter()
                    .map(|member| member.session)
                    .filter(|id| *id != self.me);
                Ok(self.members(ids).await)
            }
        }
    }

    /// Look up members, dropping any that have gone. A message to a member that
    /// has departed is not an error — it is the ordinary state of a swarm that
    /// is finishing.
    async fn members(&self, ids: impl Iterator<Item = SessionId>) -> Vec<SwarmMember> {
        let mut out = Vec::new();
        for id in ids {
            if let Some(member) = self.host.swarms().member(&self.swarm, id).await {
                out.push(member);
            }
        }
        out
    }

    /// Deliver to one member, returning whether it landed.
    async fn deliver(&self, to: &SwarmMember, message: &PeerMessage, from: &str) -> bool {
        // A member that has already finished has no turn to reach and will
        // never start another. Delivering to it would be reported as success.
        if !to.status.is_active() {
            return false;
        }
        let Some(host) = self.host.host(to.session).await else {
            return false;
        };
        let body = render(from, message);

        match message.delivery {
            // Notify still enqueues: attached clients see the event when the
            // recipient's next turn drains it. The difference from `interrupt`
            // is urgency, not whether it arrives — a message that is silently
            // dropped because nobody was looking is a message that never
            // existed.
            Delivery::Notify => host.inject(SoftInterrupt {
                content: body,
                source: SoftInterruptSource::System,
                urgent: false,
            }),
            Delivery::Interrupt => host.inject(SoftInterrupt {
                content: body,
                source: SoftInterruptSource::System,
                urgent: true,
            }),
            Delivery::Wake => {
                if host.is_busy() {
                    host.inject(SoftInterrupt {
                        content: body,
                        source: SoftInterruptSource::System,
                        urgent: true,
                    });
                } else {
                    // Idle: there is no turn to interrupt, so start one. Spawned
                    // rather than awaited — a sender must not block for as long
                    // as the recipient takes to answer.
                    let host = Arc::clone(&host);
                    tokio::spawn(async move {
                        host.drive(&body).await;
                    });
                }
            }
        }
        true
    }

    /// This session's own name, for the "from" line.
    async fn my_name(&self) -> String {
        self.host
            .swarms()
            .member(&self.swarm, self.me)
            .await
            .map_or_else(|| self.me.to_string(), |member| member.name)
    }
}

/// Render a message as the recipient sees it.
///
/// The sender is named first and the summary before the body, because the
/// recipient is a model mid-task deciding whether to break off — it should be
/// able to make that decision from the first line.
fn render(from: &str, message: &PeerMessage) -> String {
    let mut out = format!("Message from {from}");
    if let Some(tldr) = &message.tldr {
        out.push_str(&format!(" — {}", tldr.trim()));
    }
    out.push_str(":\n\n");
    out.push_str(message.body.trim());
    out
}

/// The tool-facing view.
impl SwarmPeers for SessionPeers {
    fn identity<'a>(
        &'a self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = SwarmIdentity> + Send + 'a>> {
        Box::pin(async move {
            SwarmIdentity {
                session: self.me.to_string(),
                name: self.my_name().await,
                is_coordinator: self
                    .host
                    .swarms()
                    .is_coordinator(&self.swarm, self.me)
                    .await,
            }
        })
    }

    fn roster<'a>(
        &'a self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<PeerSummary>> + Send + 'a>> {
        Box::pin(async move {
            let swarms = self.host.swarms();
            let mine: std::collections::HashSet<SessionId> = swarms
                .subtree(&self.swarm, self.me)
                .await
                .into_iter()
                .collect();
            swarms
                .members(&self.swarm)
                .await
                .into_iter()
                .filter(|member| member.session != self.me)
                .map(|member| PeerSummary {
                    session: member.session.to_string(),
                    name: member.name,
                    role: match member.role {
                        MemberRole::Coordinator => "coordinator".to_string(),
                        MemberRole::Worker => "worker".to_string(),
                        MemberRole::Peer => "peer".to_string(),
                    },
                    status: status_word(&member.status),
                    in_my_subtree: mine.contains(&member.session),
                })
                .collect()
        })
    }

    fn send<'a>(
        &'a self,
        message: &'a PeerMessage,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Delivered, String>> + Send + 'a>>
    {
        Box::pin(async move {
            let targets = self.recipients(&message.audience).await?;
            let from = self.my_name().await;
            let mut recipients = Vec::new();
            for target in targets {
                if self.deliver(&target, message, &from).await {
                    recipients.push(target.name);
                }
            }
            recipients.sort();
            Ok(Delivered {
                reached: recipients.len(),
                recipients,
            })
        })
    }
}

fn status_word(status: &MemberStatus) -> String {
    match status {
        MemberStatus::Active => "active",
        MemberStatus::Finished => "finished",
        MemberStatus::Failed { .. } => "failed",
        MemberStatus::Departed => "gone",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(audience: Audience, body: &str) -> PeerMessage {
        PeerMessage {
            audience,
            tldr: None,
            body: body.to_string(),
            delivery: Delivery::Notify,
        }
    }

    #[test]
    fn a_rendered_message_names_the_sender_first() {
        let rendered = render(
            "reviewer",
            &message(Audience::Subtree, "parse.rs is broken"),
        );
        assert!(rendered.starts_with("Message from reviewer"));
        assert!(rendered.contains("parse.rs is broken"));
    }

    #[test]
    fn a_summary_appears_on_the_first_line_where_it_is_useful() {
        let mut with_tldr = message(Audience::Subtree, "a long explanation");
        with_tldr.tldr = Some("stop editing parse.rs".into());
        let rendered = render("lead", &with_tldr);
        let first_line = rendered.lines().next().unwrap_or_default();
        assert!(
            first_line.contains("stop editing parse.rs"),
            "the recipient decides whether to break off from line one: {first_line}"
        );
    }

    #[test]
    fn every_member_status_has_a_word_for_the_model() {
        assert_eq!(status_word(&MemberStatus::Active), "active");
        assert_eq!(status_word(&MemberStatus::Finished), "finished");
        assert_eq!(
            status_word(&MemberStatus::Failed {
                reason: "boom".into()
            }),
            "failed"
        );
        assert_eq!(status_word(&MemberStatus::Departed), "gone");
    }
}
