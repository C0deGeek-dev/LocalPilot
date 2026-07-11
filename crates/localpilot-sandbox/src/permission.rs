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
    /// A launch mode that approves everything with no prompts. Never the
    /// default. Path-bearing effects (the file tools) keep the workspace
    /// boundary — an out-of-workspace path prompts interactively and is denied
    /// headless, so bypass is never weaker than `default` — and
    /// redaction/logging stay on. Shell commands carry no path information, so
    /// bypass auto-allows any command class; a command's own file access is
    /// not contained. See docs/07 §Permission Profiles.
    Bypass,
    /// A launch mode that approves everything, including out-of-workspace
    /// paths, with no prompts at all. The user explicitly accepts full
    /// responsibility. Never the default; must be set explicitly per launch or
    /// per project config. Redaction and logging stay on. See docs/07
    /// §Permission Profiles and ADR-0070.
    Unrestricted,
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

    /// A short human-readable class for an approval prompt, shared by every
    /// asking surface. Honest about why the ask exists: an in-workspace
    /// read/write can still ask (the untrusted floor turns its allow into an
    /// ask), and describing it as out-of-workspace misstates the risk to the
    /// approver.
    #[must_use]
    pub fn risk_label(self) -> &'static str {
        match self {
            Effect::ReadPath {
                secret_like: true, ..
            } => "read a secret-like path",
            Effect::ReadPath {
                inside_workspace: false,
                ..
            } => "read outside the workspace",
            Effect::ReadPath { .. } => "read a file",
            Effect::WritePath {
                inside_workspace: false,
                ..
            } => "write outside the workspace",
            Effect::WritePath {
                overwrite: true, ..
            } => "overwrite a file",
            Effect::WritePath { .. } => "write a file",
            Effect::RunCommand(_) => "run a command",
            Effect::Network => "make a network request",
        }
    }
}

/// A request to evaluate one effect.
#[derive(Debug, Clone)]
pub struct PermissionRequest {
    pub tool: String,
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
            Profile::Unrestricted => Decision::Allow,
            Profile::Bypass => {
                // Approve everything except an out-of-workspace *path* effect:
                // the workspace boundary is not silently lifted by bypass for
                // the file tools. It is lifted the same way `default` lifts it
                // — an interactive approval — never harder: a hard deny here
                // would make the most permissive prompting profile *weaker*
                // than `default` for the one effect it still gates. Commands
                // carry no path information and are allowed as-is (see the
                // Profile::Bypass docs).
                if request.effect.is_outside_workspace() {
                    ask_or_deny(request.interactivity)
                } else {
                    Decision::Allow
                }
            }
            Profile::Relaxed => {
                // The allowlist is floor-aware: it auto-approves an
                // allowlisted tool only for effects below the risk floor
                // (including non-interactive runs — this is how the ratified
                // quality gate runs headless, ADR-0009). Destructive,
                // privileged, unknown, and external-write commands,
                // secret-like reads, and out-of-workspace paths keep their
                // gate regardless of the allowlist.
                let decision = if allowlist_may_relax(request.effect)
                    && self.allowlist.iter().any(|t| t == &request.tool)
                {
                    Decision::Allow
                } else {
                    base_decision(request)
                };
                untrusted_floor(decision, request.trusted)
            }
            Profile::Default => untrusted_floor(base_decision(request), request.trusted),
        }
    }
}

/// Whether a relaxed-profile allowlist may auto-approve this effect. The
/// allowlist exists to stop prompt fatigue for routine actions; it must never
/// become a standing grant for the classes the class table exists to gate.
fn allowlist_may_relax(effect: Effect) -> bool {
    match effect {
        Effect::RunCommand(class) => matches!(
            class,
            CommandClass::ReadOnly | CommandClass::ProjectWrite | CommandClass::Network
        ),
        Effect::ReadPath {
            inside_workspace,
            secret_like,
        } => inside_workspace && !secret_like,
        Effect::WritePath {
            inside_workspace, ..
        } => inside_workspace,
        Effect::Network => true,
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

/// A shared, swappable handle to the session's [`PermissionEngine`].
///
/// The interactive host swaps profiles mid-turn (`/unrestricted` while the
/// model is generating): the runtime holds this handle and snapshots the
/// engine per tool call, so a swap applies from the very next call without
/// needing mutable access to the runtime the in-flight turn already borrows.
/// A poisoned lock recovers the inner value — a panicked writer can only have
/// completed or not completed the swap; both states are valid engines.
#[derive(Debug, Clone)]
pub struct PermissionEngineHandle {
    engine: std::sync::Arc<std::sync::RwLock<PermissionEngine>>,
}

impl PermissionEngineHandle {
    /// Wrap an engine for shared access.
    #[must_use]
    pub fn new(engine: PermissionEngine) -> Self {
        Self {
            engine: std::sync::Arc::new(std::sync::RwLock::new(engine)),
        }
    }

    /// The current engine, cloned. Callers evaluate a whole tool call against
    /// one consistent snapshot; the next call observes any swap.
    #[must_use]
    pub fn snapshot(&self) -> PermissionEngine {
        match self.engine.read() {
            Ok(engine) => engine.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Replace the engine; every subsequent snapshot sees the new one.
    pub fn set(&self, engine: PermissionEngine) {
        match self.engine.write() {
            Ok(mut current) => *current = engine,
            Err(poisoned) => *poisoned.into_inner() = engine,
        }
    }

    /// The active profile (a convenience over [`Self::snapshot`]).
    #[must_use]
    pub fn profile(&self) -> Profile {
        self.snapshot().profile()
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
    exhausted_response: bool,
}

impl ScriptedApprover {
    /// Build an approver that returns `responses` in order, then defaults to deny.
    #[must_use]
    pub fn new(responses: Vec<bool>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses.into_iter().collect()),
            exhausted_response: false,
        }
    }

    /// An approver that always approves.
    #[must_use]
    pub fn always() -> Self {
        Self {
            responses: std::sync::Mutex::new(std::collections::VecDeque::new()),
            exhausted_response: true,
        }
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
            .unwrap_or(self.exhausted_response);
        Box::pin(async move { decision })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn risk_labels_are_honest_about_workspace_scope() {
        // Bug it prevents: an untrusted-floor ask for an in-workspace read
        // being described as "read outside the workspace" — the approver was
        // shown a scarier risk than the one being decided.
        let inside_read = Effect::ReadPath {
            inside_workspace: true,
            secret_like: false,
        };
        assert_eq!(inside_read.risk_label(), "read a file");
        let outside_read = Effect::ReadPath {
            inside_workspace: false,
            secret_like: false,
        };
        assert_eq!(outside_read.risk_label(), "read outside the workspace");
        // A secret-like read reads as secret-like wherever it lives.
        let secret = Effect::ReadPath {
            inside_workspace: true,
            secret_like: true,
        };
        assert_eq!(secret.risk_label(), "read a secret-like path");
        // Writes name the boundary crossing ahead of the overwrite detail.
        let outside_overwrite = Effect::WritePath {
            inside_workspace: false,
            overwrite: true,
        };
        assert_eq!(
            outside_overwrite.risk_label(),
            "write outside the workspace"
        );
        let inside_overwrite = Effect::WritePath {
            inside_workspace: true,
            overwrite: true,
        };
        assert_eq!(inside_overwrite.risk_label(), "overwrite a file");
    }

    fn req(effect: Effect, interactivity: Interactivity, trusted: bool) -> PermissionRequest {
        PermissionRequest {
            tool: "test".to_string(),
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
    fn bypass_asks_for_out_of_workspace_paths_and_denies_them_headless() {
        // Bypass keeps the workspace boundary but is never weaker than
        // `default`: an out-of-workspace path effect prompts interactively
        // (the one prompt bypass keeps) and is denied non-interactively.
        for effect in [
            Effect::WritePath {
                inside_workspace: false,
                overwrite: false,
            },
            Effect::ReadPath {
                inside_workspace: false,
                secret_like: false,
            },
        ] {
            assert_eq!(
                engine(Profile::Bypass).decide(&req(effect, Interactivity::Interactive, true)),
                Decision::Ask,
                "effect {effect:?}"
            );
            assert_eq!(
                engine(Profile::Bypass).decide(&req(effect, Interactivity::NonInteractive, true)),
                Decision::Deny,
                "effect {effect:?}"
            );
        }
    }

    #[test]
    fn unrestricted_allows_everything_without_prompting() {
        // The user explicitly accepted full responsibility: every effect —
        // out-of-workspace paths, secret reads, destructive commands — is
        // allowed with no prompt, in both interactivity modes.
        let effects = [
            Effect::ReadPath {
                inside_workspace: false,
                secret_like: true,
            },
            Effect::WritePath {
                inside_workspace: false,
                overwrite: true,
            },
            Effect::RunCommand(CommandClass::Destructive),
            Effect::RunCommand(CommandClass::Privileged),
            Effect::Network,
        ];
        for effect in effects {
            for interactivity in [Interactivity::Interactive, Interactivity::NonInteractive] {
                // `trusted: false` too: unrestricted does not consult the trust floor.
                let decision =
                    engine(Profile::Unrestricted).decide(&req(effect, interactivity, false));
                assert_eq!(
                    decision,
                    Decision::Allow,
                    "effect {effect:?} {interactivity:?}"
                );
            }
        }
    }

    #[test]
    fn relaxed_allowlist_auto_approves_listed_tools() {
        let e = PermissionEngine::new(Profile::Relaxed, vec!["run_shell".to_string()]);
        let mut request = req(
            Effect::RunCommand(CommandClass::ProjectWrite),
            Interactivity::Interactive,
            true,
        );
        request.tool = "run_shell".to_string();
        assert_eq!(e.decide(&request), Decision::Allow);
        // A non-listed tool still follows the table.
        request.tool = "write_file".to_string();
        assert_eq!(e.decide(&request), Decision::Ask);
    }

    #[test]
    fn allowlist_never_lifts_destructive_privileged_or_unknown_commands() {
        // Allowlisting `run_shell` (the obvious prompt-fatigue move) must not
        // auto-approve the classes the class table exists to gate.
        let e = PermissionEngine::new(Profile::Relaxed, vec!["run_shell".to_string()]);
        for class in [
            CommandClass::Destructive,
            CommandClass::Privileged,
            CommandClass::Unknown,
            CommandClass::ExternalWrite,
        ] {
            let mut request = req(Effect::RunCommand(class), Interactivity::Interactive, true);
            request.tool = "run_shell".to_string();
            assert_eq!(e.decide(&request), Decision::Ask, "class {class:?}");

            let mut request = req(
                Effect::RunCommand(class),
                Interactivity::NonInteractive,
                true,
            );
            request.tool = "run_shell".to_string();
            assert_eq!(e.decide(&request), Decision::Deny, "class {class:?}");
        }
    }

    #[test]
    fn allowlist_still_relaxes_the_low_risk_command_classes() {
        // Both interactive and non-interactive: the ratified headless quality
        // gate (ADR-0009) relies on the non-interactive allowance.
        let e = PermissionEngine::new(Profile::Relaxed, vec!["run_shell".to_string()]);
        for class in [
            CommandClass::ReadOnly,
            CommandClass::ProjectWrite,
            CommandClass::Network,
        ] {
            for interactivity in [Interactivity::Interactive, Interactivity::NonInteractive] {
                let mut request = req(Effect::RunCommand(class), interactivity, true);
                request.tool = "run_shell".to_string();
                assert_eq!(
                    e.decide(&request),
                    Decision::Allow,
                    "class {class:?} {interactivity:?}"
                );
            }
        }
    }

    #[test]
    fn allowlist_never_lifts_secret_reads_or_out_of_workspace_paths() {
        let e = PermissionEngine::new(Profile::Relaxed, vec!["read_file".to_string()]);
        let mut secret = req(
            Effect::ReadPath {
                inside_workspace: true,
                secret_like: true,
            },
            Interactivity::Interactive,
            true,
        );
        secret.tool = "read_file".to_string();
        assert_eq!(e.decide(&secret), Decision::Ask);

        let mut outside = req(
            Effect::ReadPath {
                inside_workspace: false,
                secret_like: false,
            },
            Interactivity::Interactive,
            true,
        );
        outside.tool = "read_file".to_string();
        assert_eq!(e.decide(&outside), Decision::Ask);
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

    #[test]
    fn engine_handle_swaps_are_visible_to_later_snapshots() {
        let handle = PermissionEngineHandle::new(engine(Profile::Default));
        let clone = handle.clone();
        assert_eq!(handle.profile(), Profile::Default);

        // A swap through one clone (the host's mid-turn slash command) is
        // observed by the next snapshot through the other (the runtime's next
        // tool call).
        clone.set(engine(Profile::Unrestricted));
        assert_eq!(handle.profile(), Profile::Unrestricted);
        let decision = handle.snapshot().decide(&req(
            Effect::ReadPath {
                inside_workspace: false,
                secret_like: false,
            },
            Interactivity::NonInteractive,
            true,
        ));
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn scripted_approver_always_approves_after_exhaustion() {
        let approver = ScriptedApprover::always();
        let request = req(
            Effect::RunCommand(CommandClass::ProjectWrite),
            Interactivity::Interactive,
            true,
        );

        assert!(futures::executor::block_on(approver.approve(&request)));
        assert!(futures::executor::block_on(approver.approve(&request)));
    }

    #[test]
    fn scripted_approver_new_denies_after_script_is_exhausted() {
        let approver = ScriptedApprover::new(vec![true]);
        let request = req(
            Effect::RunCommand(CommandClass::ProjectWrite),
            Interactivity::Interactive,
            true,
        );

        assert!(futures::executor::block_on(approver.approve(&request)));
        assert!(!futures::executor::block_on(approver.approve(&request)));
    }
}
