//! What evidence task guidance asks for, and what was actually observed.
//!
//! Guidance says which observations would support a claim: a command's exit
//! status and output, a service's response, an interaction, a rendered image.
//! This record keeps that request apart from what happened. Each observation
//! comes from a tool's typed result (a shell exit code, a permission refusal,
//! an image attached to a result), never from the tool's text, and is linked
//! to the component and intent of the selection in force and to the call that
//! produced it.
//!
//! Loading guidance is not an observation, and neither is a tool being
//! available. A capability the session lacks can only be reported as
//! unavailable. An image counts as delivered when a request holding it went to
//! a provider that accepts images, and as inspected only once that provider
//! answered the request; an image sent to a text-only provider stays captured.
//!
//! The record is advisory and in memory. It changes no check, cadence,
//! permission or completion rule; the verification gate keeps owning those.

use std::collections::BTreeSet;

use localpilot_core::{ContentBlock, Message, ToolUseId};

/// The most observations kept.
const MAX_OBSERVATIONS: usize = 256;

/// The state of an observation or of a requested capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceState {
    /// Asked for by guidance; nothing observed yet.
    Planned,
    /// The session has no tool for it.
    Unavailable,
    /// A tool call for it was refused by permission.
    Denied,
    /// It ran and reported failure (a non-zero exit, an error result).
    Failed,
    /// It ran and reported success. Says what happened, not that the task's
    /// behaviour is verified.
    Passed,
}

/// How far an image got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageState {
    /// Produced by a tool; not yet in a request to an image-capable provider.
    Captured,
    /// In a request to a provider that accepts images.
    Delivered,
    /// That provider answered the request that held it.
    Inspected,
}

/// One observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// The capability it bears on (`process.capture`, `image.inspect`, ...).
    pub capability: String,
    /// The tool and call that produced it.
    pub tool: String,
    pub call: String,
    /// The component and first intent of the selection in force, if any.
    pub component: Option<String>,
    pub intent: Option<String>,
    pub state: EvidenceState,
    /// Set for an image.
    pub image: Option<ImageState>,
    /// What was observed, in a few words (`exit 2`, `refused`).
    pub result: String,
}

/// A capability guidance asked for, and how it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestedEvidence {
    pub capability: String,
    /// The guidance that asked for it.
    pub requested_by: Vec<String>,
    pub state: EvidenceState,
    /// The calls whose observations decided the state.
    pub calls: Vec<String>,
}

/// What one tool call produced, as the session saw it.
pub(crate) struct ToolFacts<'a> {
    pub(crate) tool: &'a str,
    pub(crate) call: &'a ToolUseId,
    pub(crate) is_error: bool,
    pub(crate) denied: bool,
    pub(crate) exit_code: Option<i32>,
    pub(crate) has_image: bool,
}

/// The session's evidence record.
#[derive(Default)]
pub(crate) struct TaskEvidence {
    observations: Vec<Observation>,
    /// Image observations sent in the request now awaiting its answer.
    awaiting: Vec<String>,
}

impl TaskEvidence {
    /// Record what a tool call produced, linked to `selection`.
    pub(crate) fn tool_finished(
        &mut self,
        facts: &ToolFacts<'_>,
        selection: Option<&crate::GuidanceSelection>,
    ) {
        let Some(capability) = capability_of(facts.tool) else {
            return;
        };
        let (state, result) = if facts.denied {
            (EvidenceState::Denied, "refused by permission".to_string())
        } else if let Some(code) = facts.exit_code {
            if code == 0 {
                (EvidenceState::Passed, "exit 0".to_string())
            } else {
                (EvidenceState::Failed, format!("exit {code}"))
            }
        } else if facts.is_error {
            (EvidenceState::Failed, "error result".to_string())
        } else {
            (EvidenceState::Passed, "result returned".to_string())
        };
        let image =
            (capability == "image.inspect" && facts.has_image && state == EvidenceState::Passed)
                .then_some(ImageState::Captured);
        if self.observations.len() == MAX_OBSERVATIONS {
            self.observations.remove(0);
        }
        self.observations.push(Observation {
            capability: capability.to_string(),
            tool: facts.tool.to_string(),
            call: facts.call.to_string(),
            component: selection.map(|s| s.component.component.clone()),
            intent: selection
                .and_then(|s| s.intent.primary())
                .map(|i| i.as_str().to_string()),
            state,
            image,
            result: if image.is_some() {
                "image captured".to_string()
            } else {
                result
            },
        });
    }

    /// A request is being sent: images it holds reach the provider only when
    /// the provider accepts images.
    pub(crate) fn request_sent(&mut self, messages: &[Message], accepts_images: bool) {
        self.awaiting.clear();
        if !accepts_images {
            return;
        }
        let held: BTreeSet<String> = messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|block| match block {
                ContentBlock::ToolResult(result) if result.image.is_some() => {
                    Some(result.id.to_string())
                }
                _ => None,
            })
            .collect();
        for observation in &mut self.observations {
            if observation.image == Some(ImageState::Captured) && held.contains(&observation.call) {
                observation.image = Some(ImageState::Delivered);
                observation.result = "image delivered".to_string();
                self.awaiting.push(observation.call.clone());
            }
        }
    }

    /// The provider answered the request last sent.
    pub(crate) fn response_received(&mut self) {
        for call in std::mem::take(&mut self.awaiting) {
            for observation in &mut self.observations {
                if observation.call == call && observation.image == Some(ImageState::Delivered) {
                    observation.image = Some(ImageState::Inspected);
                    observation.result = "image inspected".to_string();
                }
            }
        }
    }

    pub(crate) fn observations(&self) -> Vec<Observation> {
        self.observations.clone()
    }
}

/// The capability a tool's result bears on. Tools that only read the
/// workspace or load guidance bear on none.
fn capability_of(tool: &str) -> Option<&'static str> {
    match tool {
        "run_shell" => Some("process.capture"),
        "run_background" => Some("process.background"),
        "fetch" => Some("http.observe"),
        "read_image" => Some("image.inspect"),
        // Browser adapter tools, by what they do.
        name if name.contains("screenshot") => Some("image.inspect"),
        name if [
            "navigate",
            "click",
            "fill",
            "type_text",
            "press_key",
            "hover",
            "select_option",
        ]
        .iter()
        .any(|action| name.contains(action)) =>
        {
            Some("browser.interact")
        }
        _ => None,
    }
}

/// Whether a capability's route can be used in this session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteStatus {
    /// These tools provide it.
    Available(Vec<String>),
    /// LocalPilot has a route, but not in this session, for this reason.
    Unavailable(String),
    /// No route is qualified in LocalPilot yet; claims that need it cannot be
    /// made anywhere.
    Deferred(String),
}

/// One observation capability and its route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationRoute {
    pub capability: &'static str,
    pub status: RouteStatus,
}

/// Every observation capability guidance can ask for, and whether this
/// session can provide it, given its tool names and whether its provider
/// accepts images.
#[must_use]
pub fn observation_routes(names: &[&str], accepts_images: bool) -> Vec<ObservationRoute> {
    let tools = |pick: &dyn Fn(&str) -> bool| -> Vec<String> {
        names
            .iter()
            .filter(|n| pick(n))
            .map(|n| (*n).to_string())
            .collect()
    };
    let available = |list: Vec<String>, missing: &str| {
        if list.is_empty() {
            RouteStatus::Unavailable(missing.to_string())
        } else {
            RouteStatus::Available(list)
        }
    };
    let browser = {
        let capabilities = crate::capabilities_from_tools(names);
        if capabilities.contains("browser.interact") {
            RouteStatus::Available(tools(&|n| capability_of(n) == Some("browser.interact")))
        } else {
            RouteStatus::Unavailable(
                "no browser tools (navigate and click) are connected".to_string(),
            )
        }
    };
    let image_tools = tools(&|n| capability_of(n) == Some("image.inspect"));
    let image = if image_tools.is_empty() {
        RouteStatus::Unavailable("no tool returns images".to_string())
    } else if !accepts_images {
        RouteStatus::Unavailable(
            "the provider does not accept images; an image can be captured but not inspected"
                .to_string(),
        )
    } else {
        RouteStatus::Available(image_tools)
    };
    vec![
        ObservationRoute {
            capability: "process.capture",
            status: available(tools(&|n| n == "run_shell"), "run_shell is not registered"),
        },
        ObservationRoute {
            capability: "process.background",
            status: available(tools(&|n| n == "run_background"), "run_background is not registered"),
        },
        ObservationRoute {
            capability: "http.observe",
            status: RouteStatus::Unavailable(if names.contains(&"fetch") {
                "fetch sends GET requests only; request and response checks of a service need a \
                 client that can send them"
                    .to_string()
            } else {
                "no HTTP client tool is registered".to_string()
            }),
        },
        ObservationRoute {
            capability: "browser.interact",
            status: browser,
        },
        ObservationRoute {
            capability: "image.inspect",
            status: image,
        },
        ObservationRoute {
            capability: "terminal.interact",
            status: RouteStatus::Deferred(
                "commands run buffered with stdin closed; no terminal (PTY) adapter is qualified".to_string(),
            ),
        },
        ObservationRoute {
            capability: "desktop.interact",
            status: RouteStatus::Deferred(
                "no native desktop adapter is qualified; headless tests are not window interaction".to_string(),
            ),
        },
        ObservationRoute {
            capability: "engine.run",
            status: RouteStatus::Deferred(
                "no game-engine adapter is qualified; a headless engine command is process evidence only".to_string(),
            ),
        },
    ]
}

/// How each capability asked for by the selected guidance stands, given the
/// observations and the capabilities the session has.
#[must_use]
pub fn requested_evidence(
    selection: Option<&crate::GuidanceSelection>,
    available: &BTreeSet<String>,
    observations: &[Observation],
) -> Vec<RequestedEvidence> {
    let Some(selection) = selection else {
        return Vec::new();
    };
    let mut requested: Vec<RequestedEvidence> = Vec::new();
    for supplement in &selection.supplements {
        for capability in &supplement.required_for_claim {
            match requested.iter_mut().find(|r| &r.capability == capability) {
                Some(entry) => entry.requested_by.push(supplement.name.clone()),
                None => requested.push(RequestedEvidence {
                    capability: capability.clone(),
                    requested_by: vec![supplement.name.clone()],
                    state: EvidenceState::Planned,
                    calls: Vec::new(),
                }),
            }
        }
    }
    for entry in &mut requested {
        if !available.contains(&entry.capability) {
            // No tool for it: whatever ran, this claim cannot be supported.
            entry.state = EvidenceState::Unavailable;
            continue;
        }
        let relevant: Vec<&Observation> = observations
            .iter()
            .filter(|o| o.capability == entry.capability)
            .filter(|o| o.component.as_deref() == Some(selection.component.component.as_str()))
            .collect();
        entry.calls = relevant.iter().map(|o| o.call.clone()).collect();
        // An image supports a visual claim only once inspected.
        entry.state = relevant
            .iter()
            .rev()
            .find(|o| o.image.is_none() || o.image == Some(ImageState::Inspected))
            .map_or(EvidenceState::Planned, |o| o.state);
    }
    requested
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts<'a>(tool: &'a str, call: &'a ToolUseId) -> ToolFacts<'a> {
        ToolFacts {
            tool,
            call,
            is_error: false,
            denied: false,
            exit_code: None,
            has_image: false,
        }
    }

    #[test]
    fn a_tool_that_reads_or_loads_guidance_is_not_evidence() {
        let mut record = TaskEvidence::default();
        let call = ToolUseId::from("c1");
        for tool in ["read_file", "skill_load", "grep", "active_skills"] {
            record.tool_finished(&facts(tool, &call), None);
        }
        assert!(record.observations().is_empty());
    }

    #[test]
    fn exit_codes_and_refusals_decide_the_state() {
        let mut record = TaskEvidence::default();
        let call = ToolUseId::from("c1");
        record.tool_finished(
            &ToolFacts {
                exit_code: Some(0),
                ..facts("run_shell", &call)
            },
            None,
        );
        record.tool_finished(
            &ToolFacts {
                exit_code: Some(2),
                ..facts("run_shell", &call)
            },
            None,
        );
        record.tool_finished(
            &ToolFacts {
                denied: true,
                ..facts("run_shell", &call)
            },
            None,
        );
        let states: Vec<_> = record
            .observations()
            .iter()
            .map(|o| (o.state, o.result.clone()))
            .collect();
        assert_eq!(
            states,
            [
                (EvidenceState::Passed, "exit 0".to_string()),
                (EvidenceState::Failed, "exit 2".to_string()),
                (EvidenceState::Denied, "refused by permission".to_string()),
            ]
        );
    }

    #[test]
    fn an_image_is_inspected_only_after_an_image_capable_provider_answered() {
        let call = ToolUseId::from("img");
        let message = Message::new(
            localpilot_core::Role::Tool,
            vec![ContentBlock::ToolResult({
                let mut result = localpilot_core::ToolResult::success(call.clone(), "image");
                result.image = Some(localpilot_core::ToolImage {
                    media_type: "image/png".into(),
                    data: "AA==".into(),
                });
                result
            })],
        );

        let mut text_only = TaskEvidence::default();
        text_only.tool_finished(
            &ToolFacts {
                has_image: true,
                ..facts("read_image", &call)
            },
            None,
        );
        text_only.request_sent(std::slice::from_ref(&message), false);
        text_only.response_received();
        assert_eq!(
            text_only.observations()[0].image,
            Some(ImageState::Captured)
        );

        let mut vision = TaskEvidence::default();
        vision.tool_finished(
            &ToolFacts {
                has_image: true,
                ..facts("read_image", &call)
            },
            None,
        );
        vision.request_sent(std::slice::from_ref(&message), true);
        assert_eq!(vision.observations()[0].image, Some(ImageState::Delivered));
        vision.response_received();
        assert_eq!(vision.observations()[0].image, Some(ImageState::Inspected));
    }
}
