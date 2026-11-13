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
/// The most shell runs kept for recipe drafts.
const MAX_RUNS: usize = 64;

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

/// A shell command that ran, kept so a person can turn a successful one into
/// a reviewable recipe draft. Holds nothing a secret could sit in: arguments
/// are redacted when recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    /// The tool call that ran it.
    pub call: String,
    /// What it ran, for display, redacted.
    pub command: String,
    /// The program and arguments, when the command is plain argv. `None` when
    /// it uses shell syntax, which a recipe cannot express.
    pub argv: Option<(String, Vec<String>)>,
    /// Why there is no argv.
    pub not_argv_reason: Option<String>,
    /// The command only reads (a listing, a status, a version probe).
    pub read_only: bool,
    /// Whether it exited 0.
    pub succeeded: bool,
    /// The component of the selection in force when it ran.
    pub component: Option<String>,
    /// File hashes taken when it finished, so a draft never claims a
    /// validation that did not happen.
    pub file_hashes: Vec<(String, String)>,
}

/// The program and arguments of a `run_shell` input, or why there are none.
pub(crate) fn argv_of(input: &serde_json::Value) -> Result<(String, Vec<String>), String> {
    let redact = |text: &str| localpilot_config::redact::redact(text);
    if let Some(program) = input.get("program").and_then(|p| p.as_str()) {
        let args: Vec<String> = input
            .get("args")
            .and_then(|a| a.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(redact))
                    .collect()
            })
            .unwrap_or_default();
        return Ok((redact(program), args));
    }
    let Some(command) = input.get("command").and_then(|c| c.as_str()) else {
        return Err("the call has no command".to_string());
    };
    // A recipe is a program and an argument array. Only a command that is
    // already plain words can be split without changing what it means.
    if let Some(bad) = command.chars().find(|c| {
        matches!(
            c,
            '|' | '&'
                | ';'
                | '<'
                | '>'
                | '`'
                | '$'
                | '('
                | ')'
                | '"'
                | '\''
                | '\\'
                | '*'
                | '?'
                | '{'
                | '}'
                | '#'
                | '~'
                | '!'
                | '\n'
                | '\r'
        )
    }) {
        return Err(format!(
            "the command uses shell syntax ({}); a recipe is a program and an argument list",
            if bad == '\n' {
                "a line break".to_string()
            } else {
                format!("`{bad}`")
            }
        ));
    }
    let mut words = command.split_whitespace().map(str::to_string);
    let Some(program) = words.next() else {
        return Err("the command is empty".to_string());
    };
    if program.contains('=') {
        return Err(
            "the command starts with an environment assignment; name the variable in `env` instead"
                .to_string(),
        );
    }
    Ok((redact(&program), words.map(|w| redact(&w)).collect()))
}

/// A version or help probe: it reads and says nothing about how to run the
/// project, so it is never worth a recipe.
fn is_probe(program: &str, args: &[String]) -> bool {
    !program.contains(['/', '\\'])
        && matches!(args, [arg] if matches!(arg.as_str(), "--version" | "-V" | "--help" | "-h"))
}

/// What one tool call produced, as the session saw it.
pub(crate) struct ToolFacts<'a> {
    pub(crate) tool: &'a str,
    pub(crate) call: &'a ToolUseId,
    pub(crate) is_error: bool,
    pub(crate) denied: bool,
    pub(crate) exit_code: Option<i32>,
    pub(crate) has_image: bool,
    /// The call's input, for a shell command's argv.
    pub(crate) input: Option<&'a serde_json::Value>,
    /// The workspace root, for the files a run's validity depends on.
    pub(crate) root: Option<&'a std::path::Path>,
}

/// The session's evidence record.
#[derive(Default)]
pub(crate) struct TaskEvidence {
    observations: Vec<Observation>,
    runs: Vec<RunRecord>,
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
        if facts.tool == "run_shell" && !facts.denied {
            self.record_run(facts, state, selection);
        }
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

    fn record_run(
        &mut self,
        facts: &ToolFacts<'_>,
        state: EvidenceState,
        selection: Option<&crate::GuidanceSelection>,
    ) {
        let Some(input) = facts.input else {
            return;
        };
        let (argv, reason) = match argv_of(input) {
            Ok(argv) => (Some(argv), None),
            Err(reason) => (None, Some(reason)),
        };
        let command = match &argv {
            Some((program, args)) => std::iter::once(program.as_str())
                .chain(args.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" "),
            None => localpilot_config::redact::redact(
                input
                    .get("command")
                    .and_then(|c| c.as_str())
                    .unwrap_or_default(),
            ),
        };
        let succeeded = state == EvidenceState::Passed && facts.exit_code == Some(0);
        let component = selection.map(|s| s.component.component.clone());
        // Hashed when the command finishes, and only for a run that could be
        // turned into a draft.
        let file_hashes = match (facts.root, succeeded) {
            (Some(root), true) => {
                localpilot_skills::validation_hashes(root, component.as_deref().unwrap_or("."))
            }
            _ => Vec::new(),
        };
        if self.runs.len() == MAX_RUNS {
            self.runs.remove(0);
        }
        let read_only = localpilot_tools::shell_call_is_read_only(input)
            || argv
                .as_ref()
                .is_some_and(|(program, args)| is_probe(program, args));
        self.runs.push(RunRecord {
            call: facts.call.to_string(),
            command,
            argv,
            not_argv_reason: reason,
            read_only,
            succeeded,
            component,
            file_hashes,
        });
    }

    pub(crate) fn runs(&self) -> Vec<RunRecord> {
        self.runs.clone()
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
        // Browser adapter tools, by what they do. Whole words only: a tool
        // named `clickup_create_task` is not a click.
        name if has_word(name, "screenshot") => Some("image.inspect"),
        name if ["navigate", "click", "fill", "hover"]
            .iter()
            .any(|action| has_word(name, action)) =>
        {
            Some("browser.interact")
        }
        // Reading the page back (its structure, its console, its requests) is
        // how state and errors are captured. Only for browser tools.
        name if has_word(name, "browser")
            && ["snapshot", "console", "network", "evaluate"]
                .iter()
                .any(|action| has_word(name, action)) =>
        {
            Some("browser.interact")
        }
        _ => None,
    }
}

/// Whether `word` is one of the `_`-, `-`- or `.`-separated words of a tool name.
pub(crate) fn has_word(name: &str, word: &str) -> bool {
    name.split(['_', '-', '.'])
        .any(|part| part.eq_ignore_ascii_case(word))
}

/// Whether a capability's route can be used in this session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteStatus {
    /// These tools provide it, and the model is offered them now.
    Available(Vec<String>),
    /// These tools provide it and are registered, but the broker does not
    /// advertise them yet: the model reaches them with `tool_search` and
    /// `tool_load`, and a call to one before then does not run.
    Hidden(Vec<String>),
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
/// session can provide it, given its tool names, whether its provider accepts
/// images, and whether the broker currently advertises a tool.
#[must_use]
pub fn observation_routes(
    names: &[&str],
    accepts_images: bool,
    is_advertised: &dyn Fn(&str) -> bool,
) -> Vec<ObservationRoute> {
    let tools = |pick: &dyn Fn(&str) -> bool| -> Vec<String> {
        names
            .iter()
            .filter(|n| pick(n))
            .map(|n| (*n).to_string())
            .collect()
    };
    // A route is available once the model is offered what it needs: any one
    // tool, or for a route with essential actions (a browser must both
    // navigate and click) one advertised tool for each. Otherwise it is hidden
    // behind `tool_load`, and the tools that would complete it are named.
    let route = |list: Vec<String>, missing: &str, essential: &[&str]| {
        if list.is_empty() {
            return RouteStatus::Unavailable(missing.to_string());
        }
        let advertised: Vec<String> = list.iter().filter(|t| is_advertised(t)).cloned().collect();
        let needed: Vec<&str> = if essential.is_empty() {
            vec![""]
        } else {
            essential.to_vec()
        };
        let lacking: Vec<&str> = needed
            .iter()
            .copied()
            .filter(|word| {
                !advertised
                    .iter()
                    .any(|tool| word.is_empty() || has_word(tool, word))
            })
            .collect();
        if lacking.is_empty() {
            return RouteStatus::Available(advertised);
        }
        let hidden: Vec<String> = list
            .into_iter()
            .filter(|tool| !is_advertised(tool))
            .filter(|tool| {
                lacking
                    .iter()
                    .any(|word| word.is_empty() || has_word(tool, word))
            })
            .collect();
        RouteStatus::Hidden(hidden)
    };
    let browser = {
        let capabilities = crate::capabilities_from_tools(names);
        if capabilities.contains("browser.interact") {
            route(
                tools(&|n| capability_of(n) == Some("browser.interact")),
                "no browser tools (navigate and click) are connected",
                &["navigate", "click"],
            )
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
        route(image_tools, "no tool returns images", &[])
    };
    vec![
        ObservationRoute {
            capability: "process.capture",
            status: route(tools(&|n| n == "run_shell"), "run_shell is not registered", &[]),
        },
        ObservationRoute {
            capability: "process.background",
            status: route(tools(&|n| n == "run_background"), "run_background is not registered", &[]),
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
            input: None,
            root: None,
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
    fn tool_names_are_classified_by_whole_words() {
        assert_eq!(capability_of("browser_click"), Some("browser.interact"));
        assert_eq!(capability_of("navigate_page"), Some("browser.interact"));
        assert_eq!(capability_of("take_screenshot"), Some("image.inspect"));
        assert_eq!(
            capability_of("playwright_browser_take_screenshot"),
            Some("image.inspect")
        );
        // A word inside another word is not that word.
        assert_eq!(capability_of("clickup_create_task"), None);
        // State and errors are read back through browser tools only.
        assert_eq!(capability_of("browser_snapshot"), Some("browser.interact"));
        assert_eq!(
            capability_of("browser_console_messages"),
            Some("browser.interact")
        );
        assert_eq!(
            capability_of("browser_network_requests"),
            Some("browser.interact")
        );
        assert_eq!(capability_of("network_status"), None);
        assert_eq!(capability_of("console_log"), None);
        assert_eq!(capability_of("refill_order"), None);
        let caps = crate::capabilities_from_tools(&["clickup_create_task", "navigate_page"]);
        assert!(!caps.contains("browser.interact"));
    }

    #[test]
    fn a_route_is_available_only_when_its_essential_tools_are_advertised() {
        let names = [
            "browser_navigate",
            "browser_click",
            "browser_snapshot",
            "run_shell",
        ];
        let route = |advertised: &[&str]| {
            let advertised: Vec<String> = advertised.iter().map(|n| (*n).to_string()).collect();
            observation_routes(&names, true, &move |name| {
                advertised.iter().any(|a| a == name)
            })
            .into_iter()
            .find(|r| r.capability == "browser.interact")
            .unwrap()
            .status
        };
        // Everything advertised.
        assert!(matches!(route(&names), RouteStatus::Available(_)));
        // Click is advertised, navigation is not: hidden, and navigation named.
        assert_eq!(
            route(&["browser_click"]),
            RouteStatus::Hidden(vec!["browser_navigate".to_string()])
        );
        // Neither: both essentials named, the optional snapshot tool is not.
        assert_eq!(
            route(&[]),
            RouteStatus::Hidden(vec![
                "browser_navigate".to_string(),
                "browser_click".to_string()
            ])
        );
        // Both essentials: available even though snapshot is still hidden.
        assert!(matches!(
            route(&["browser_navigate", "browser_click"]),
            RouteStatus::Available(_)
        ));
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

#[cfg(test)]
mod argv_tests {
    use super::argv_of;
    use serde_json::json;

    #[test]
    fn plain_commands_split_and_shell_syntax_is_refused() {
        let (program, args) = argv_of(&json!({"command": "cargo build --release -p app"})).unwrap();
        assert_eq!((program.as_str(), args.len()), ("cargo", 4));
        let (program, args) = argv_of(
            &json!({"program": "dotnet", "args": ["run", "--urls", "http://127.0.0.1:5199"]}),
        )
        .unwrap();
        assert_eq!((program.as_str(), args.len()), ("dotnet", 3));
        for command in [
            "cargo build && cargo test",
            "cargo test | tee out.txt",
            "cargo test > out.txt",
            "echo $HOME",
            "cargo build; cargo test",
            "cargo test 'a b'",
            "cargo build\ncargo test",
        ] {
            let reason = argv_of(&json!({ "command": command })).unwrap_err();
            assert!(reason.contains("shell syntax"), "{command}: {reason}");
        }
        let env = argv_of(&json!({"command": "RUST_LOG=debug cargo run"})).unwrap_err();
        assert!(env.contains("environment assignment"), "{env}");
        assert!(argv_of(&json!({"command": "  "})).is_err());
    }

    #[test]
    fn credential_shaped_arguments_are_redacted_when_recorded() {
        let (_, args) = argv_of(&json!({
            "program": "tool",
            "args": ["--key", "sk-abcdefghijklmnopqrstuvwxyz0123456789"]
        }))
        .unwrap();
        assert!(
            !args
                .join(" ")
                .contains("sk-abcdefghijklmnopqrstuvwxyz0123456789"),
            "{args:?}"
        );
    }
}
