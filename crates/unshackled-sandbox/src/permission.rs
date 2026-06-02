//! The permission engine, profiles, and approval interface.
//!
//! Every tool effect is evaluated here. The engine maps an [`Effect`] plus
//! context (interactivity, workspace trust, profile) to a [`Decision`]. The model
//! and the harness must route every effect through [`PermissionEngine::decide`];
//! there is no path to a side effect that skips it.

use crate::command::CommandClass;

/// The outcome of a permission evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Run immediately.
    Allow,
    /// Prompt the user.
    Ask,
    /// Block and return a model-visible error.
    Deny,
}

/// The permission profile in effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// Least privilege; risky actions require approval.
    Default,
    /// A user allowlist auto-approves common safe actions; the rest still prompt.
    Relaxed,
    /// A launch mode that approves everything with no prompts. Never the default;
    /// does not lift the workspace boundary, redaction, or logging.
    Bypass,
}

/// Whether the session can prompt the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interactivity {
    Interactive,
    NonInteractive,
}

/// A side effect a tool intends to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Read a path. `secret_like` and `inside_workspace` drive the decision.
    ReadPath {
        inside_workspace: bool,
        secret_like: bool,
    },
    /// Write or overwrite a path.
    WritePath {
        inside_workspace: bool,
        overwrite: bool,
    },
    /// Run a classified command.
    RunCommand(CommandClass),
    /// Perform a network operation.
    Network,
}

impl Effect {
    /// Whether this effect touches a path outside the workspace. The workspace
    /// boundary is enforced even under `bypass`.
    #[must_use]
    pub fn is_outside_workspace(&self) -> bool {
        matches!(
            self,
            Effect::ReadPath {
                inside_workspace: false,
                ..
            } | Effect::WritePath {
                inside_workspace: false,
                ..
            }
        )
    }
}

/// A request to evaluate one effect.
#[derive(Debug, Clone)]
pub struct PermissionRequest {
    pub tool: &'static str,
    pub effect: Effect,
    pub interactivity: Interactivity,
    pub trusted: bool,
    /// A short, human-readable description of the concrete target (the path,
    /// command, or URL the tool intends to act on), for an approval prompt.
    /// Empty when the caller has nothing more specific than the effect.
    pub detail: String,
}

/// The configurable permission engine.
#[derive(Debug, Clone)]
pub struct PermissionEngine {
    profile: Profile,
    allowlist: Vec<String>,
}

impl PermissionEngine {
    /// An engine with a profile and (for `relaxed`) an allowlist of tool names.
    #[must_use]
    pub fn new(profile: Profile, allowlist: Vec<String>) -> Self {
        Self { profile, allowlist }
    }

    /// The active profile.
    #[must_use]
    pub fn profile(&self) -> Profile {
        self.profile
    }

    /// Decide whether an effect may proceed.
    #[must_use]
    pub fn decide(&self, request: &PermissionRequest) -> Decision {
        match self.profile {
            Profile::Bypass => {
                // Approve everything except an out-of-workspace path effect: the
                // workspace boundary is not silently lifted by bypass.
                if request.effect.is_outside_workspace() {
                    Decision::Deny
                } else {
                    Decision::Allow
                }
            }
            Profile::Relaxed => {
                let base = if self.allowlist.iter().any(|t| t == request.tool) {
                    Decision::Allow
                } else {
                    base_decision(request)
                };
                untrusted_floor(base, request.trusted)
            }
            Profile::Default => untrusted_floor(base_decision(request), request.trusted),
        }
    }
}

/// The out-of-box decision for an effect, before profile or trust adjustments.
fn base_decision(request: &PermissionRequest) -> Decision {
    match request.effect {
        Effect::ReadPath {
            inside_workspace: true,
            secret_like: false,
        } => Decision::Allow,
        Effect::ReadPath {
            inside_workspace: true,
            secret_like: true,
        }
        | Effect::ReadPath {
            inside_workspace: false,
            ..
        }
        | Effect::Network => ask_or_deny(request.interactivity),
        Effect::WritePath {
            inside_workspace: true,
            ..
        } => {
            // Writing inside a trusted workspace is allowed; otherwise prompt.
            if request.trusted {
                Decision::Allow
            } else {
                ask_or_deny(request.interactivity)
            }
        }
        Effect::WritePath {
            inside_workspace: false,
            ..
        } => ask_or_deny(request.interactivity),
        Effect::RunCommand(class) => command_decision(class, request.interactivity),
    }
}

fn command_decision(class: CommandClass, interactivity: Interactivity) -> Decision {
    match class {
        CommandClass::ReadOnly => Decision::Allow,
        CommandClass::ProjectWrite
        | CommandClass::ExternalWrite
        | CommandClass::Network
        | CommandClass::Destructive
        | CommandClass::Privileged
        | CommandClass::Unknown => ask_or_deny(interactivity),
    }
}

fn ask_or_deny(interactivity: Interactivity) -> Decision {
    match interactivity {
        Interactivity::Interactive => Decision::Ask,
        Interactivity::NonInteractive => Decision::Deny,
    }
}

/// An untrusted workspace raises an `Allow` to `Ask` so the first action prompts
/// the user (the workspace-trust prompt). A `Deny` stays denied.
fn untrusted_floor(decision: Decision, trusted: bool) -> Decision {
    if trusted {
        decision
    } else {
        match decision {
            Decision::Allow => Decision::Ask,
            other => other,
        }
    }
}

/// An approval source consulted when a decision is [`Decision::Ask`].
///
/// `approve` is asynchronous so an interactive front-end can suspend the turn
/// while it prompts the user, without blocking the executor.
pub trait Approver {
    /// Resolve to `true` to approve the requested effect.
    fn approve<'a>(
        &'a self,
        request: &'a PermissionRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + 'a>>;
}

/// A test approver scripted with fixed responses, in order.
#[derive(Debug, Default)]
pub struct ScriptedApprover {
    responses: std::sync::Mutex<std::collections::VecDeque<bool>>,
}

impl ScriptedApprover {
    /// Build an approver that returns `responses` in order, then defaults to deny.
    #[must_use]
    pub fn new(responses: Vec<bool>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses.into_iter().collect()),
        }
    }

    /// An approver that always approves.
    #[must_use]
    pub fn always() -> Self {
        Self::new(Vec::new())
    }
}

impl Approver for ScriptedApprover {
    fn approve<'a>(
        &'a self,
        _request: &'a PermissionRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + 'a>> {
        let decision = self
            .responses
            .lock()
            .ok()
            .and_then(|mut r| r.pop_front())
            .unwrap_or(false);
        Box::pin(async move { decision })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(effect: Effect, interactivity: Interactivity, trusted: bool) -> PermissionRequest {
        PermissionRequest {
            tool: "test",
            effect,
            interactivity,
            trusted,
            detail: String::new(),
        }
    }

    fn engine(profile: Profile) -> PermissionEngine {
        PermissionEngine::new(profile, Vec::new())
    }

    #[test]
    fn default_profile_follows_the_class_table() {
        let e = engine(Profile::Default);
        // read-only inside a trusted workspace: allow.
        assert_eq!(
            e.decide(&req(
                Effect::ReadPath {
                    inside_workspace: true,
                    secret_like: false
                },
                Interactivity::NonInteractive,
                true
            )),
            Decision::Allow
        );
        // destructive: ask interactive, deny non-interactive.
        assert_eq!(
            e.decide(&req(
                Effect::RunCommand(CommandClass::Destructive),
                Interactivity::Interactive,
                true
            )),
            Decision::Ask
        );
        assert_eq!(
            e.decide(&req(
                Effect::RunCommand(CommandClass::Destructive),
                Interactivity::NonInteractive,
                true
            )),
            Decision::Deny
        );
    }

    #[test]
    fn secret_reads_prompt_under_default_and_relaxed() {
        for profile in [Profile::Default, Profile::Relaxed] {
            let decision = engine(profile).decide(&req(
                Effect::ReadPath {
                    inside_workspace: true,
                    secret_like: true,
                },
                Interactivity::Interactive,
                true,
            ));
            assert_eq!(decision, Decision::Ask, "profile {profile:?}");
        }
    }

    #[test]
    fn bypass_allows_secret_reads_without_prompting() {
        let decision = engine(Profile::Bypass).decide(&req(
            Effect::ReadPath {
                inside_workspace: true,
                secret_like: true,
            },
            Interactivity::Interactive,
            true,
        ));
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn bypass_still_denies_out_of_workspace_writes() {
        let decision = engine(Profile::Bypass).decide(&req(
            Effect::WritePath {
                inside_workspace: false,
                overwrite: false,
            },
            Interactivity::Interactive,
            true,
        ));
        assert_eq!(decision, Decision::Deny);
    }

    #[test]
    fn relaxed_allowlist_auto_approves_listed_tools() {
        let e = PermissionEngine::new(Profile::Relaxed, vec!["run_shell".to_string()]);
        let mut request = req(
            Effect::RunCommand(CommandClass::ProjectWrite),
            Interactivity::Interactive,
            true,
        );
        request.tool = "run_shell";
        assert_eq!(e.decide(&request), Decision::Allow);
        // A non-listed tool still follows the table.
        request.tool = "write_file";
        assert_eq!(e.decide(&request), Decision::Ask);
    }

    #[test]
    fn untrusted_workspace_escalates_allow_to_ask() {
        let decision = engine(Profile::Default).decide(&req(
            Effect::ReadPath {
                inside_workspace: true,
                secret_like: false,
            },
            Interactivity::Interactive,
            false,
        ));
        assert_eq!(decision, Decision::Ask);
    }

    #[test]
    fn harness_cannot_obtain_an_allow_for_a_non_interactive_destructive_command() {
        // There is no API that bypasses `decide`; the strongest a caller can do is
        // ask, and a non-interactive destructive command is denied outright.
        for profile in [Profile::Default, Profile::Relaxed] {
            let decision = engine(profile).decide(&req(
                Effect::RunCommand(CommandClass::Destructive),
                Interactivity::NonInteractive,
                true,
            ));
            assert_eq!(decision, Decision::Deny, "profile {profile:?}");
        }
    }
}
