//! The shared interactive slash-command surface.
//!
//! One dependency-free source of command knowledge for every interactive host:
//! the parsed action types, the parser, and one authoritative globally-ordered
//! command+spelling catalog table that generates the inline, full-screen, and
//! pair pickers. Both hosts re-export these types so command names, descriptions,
//! order, and argument policy cannot drift between the rollback (inline) and the
//! replacement (full-screen) UIs.
//!
//! The command enum, the catalog table, and the identity list are generated from
//! one [`slash_commands!`] invocation, so a new command identity cannot exist
//! without a catalog row (and vice versa), and [`parse_slash`] dispatches on the
//! typed [`SlashCommand`] of the looked-up spelling — never on a raw string.

#![forbid(unsafe_code)]

/// Operating mode shown in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Agent,
    Harness,
    /// Research mode: a bare prompt is treated as a topic to research —
    /// local sources plus disclosed, allowlist-gated web per config
    /// (ADR-0076) — rather than a model turn.
    Research,
}

impl Mode {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Mode::Agent => "agent",
            Mode::Harness => "harness",
            Mode::Research => "research",
        }
    }
}

/// Permission profile shown in the UI. `bypass` and `unrestricted` are always
/// surfaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Default,
    Relaxed,
    Bypass,
    Unrestricted,
}

impl Profile {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Profile::Default => "default",
            Profile::Relaxed => "relaxed",
            Profile::Bypass => "BYPASS",
            Profile::Unrestricted => "UNRESTRICTED",
        }
    }
}

/// A parsed interactive slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashAction {
    SetMode(Mode),
    SetProfile(Profile),
    ToggleThinking,
    Clear,
    /// Compact the conversation. `force` trims even when it is within the
    /// configured budget (`/compact force` / `/compact_force`).
    Compact {
        force: bool,
    },
    /// Set the reasoning-effort level (validated by the host).
    SetEffort(String),
    /// Start a fresh session.
    NewSession,
    /// Branch the conversation into a new session (with a fork marker).
    Fork,
    /// Copy the conversation into a new session (no fork marker).
    CloneSession,
    /// Render the session's event tree.
    Tree,
    /// List this workspace's sessions.
    Sessions,
    /// Switch to (resume) the given session id.
    LoadSession(String),
    /// Continue the latest previous session, or a specific session when set.
    ContinueSession(Option<String>),
    /// Name (or rename) the current session (`/name` / `/rename`).
    NameSession(String),
    HarnessResume,
    WaitResume,
    /// Switch the active provider/model mid-session, or — with no provider — list
    /// the configured providers and their available models. `model` is only set
    /// when a model id follows the provider id.
    Model {
        provider: Option<String>,
        model: Option<String>,
    },
    /// Adopt a running LocalBox server into `.localpilot.toml` from inside the
    /// session (`/localbox` or `/localbox adopt`).
    LocalBoxAdopt,
    Ingest(IngestAction),
    Knowledge(String),
    /// Research a topic. `Some(topic)` runs a one-shot research pass; `None`
    /// enters persistent research mode.
    Research(Option<String>),
    ContextBuild(String),
    /// Inspect subagent definitions (`/agents [list|show <name>]`).
    Agents(String),
    /// Manage skills: sources, installs, listing.
    Skills(String),
    /// Manage background processes started this session.
    Background(BackgroundCommand),
    /// Leave interactive chat. Full-screen hosts may print the visible
    /// conversation after terminal restoration when explicitly requested.
    Exit {
        print_transcript: bool,
    },
    /// Open the help takeover. Routed for full-screen/pair only; no arguments.
    Help,
    /// Open the theme picker, or apply a theme directly when a name follows
    /// (`/theme dim`). Routed for full-screen/pair only.
    Theme(Option<String>),
    /// Open settings, optionally pre-filling the filter (`/settings mouse`).
    /// Routed for full-screen/pair only.
    Settings(Option<String>),
    /// Show the working-tree diff, optionally filtered to paths containing the
    /// given substring (`/diff src`). Routed for full-screen/pair only.
    Diff(Option<String>),
    /// Search the session timeline, optionally seeding the query (`/search foo`).
    /// Routed for full-screen/pair only.
    Search(Option<String>),
    Invalid {
        command: String,
        reason: String,
    },
    Unknown(String),
}

/// Parsed `/bg` subcommands for managing background processes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackgroundCommand {
    /// List the running background processes.
    List,
    /// Stop a single process by id.
    Stop(String),
    /// Stop every background process.
    StopAll,
}

/// Parsed ingestion slash subcommands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestAction {
    Run,
    Preview,
    Status,
    Pause,
    Resume,
    Cancel,
    Refresh,
    Rebuild,
    Skipped,
    Include(String),
    Exclude(String),
    Forget(String),
    Review,
    Promote(String),
}

/// The argument shape a spelling accepts (syntax metadata). This is the
/// authoritative syntax for the host that owns the command, including the
/// externally-routed takeovers and the pair-only `abort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgSpec {
    None,
    Optional,
    Required,
}

/// What a spelling does when it is reached with an unrecognized (stray)
/// argument, after its specific parse arm did not match. Kept distinct from
/// [`ArgSpec`] so the parser's current behaviour is preserved exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrayArgs {
    /// Reject with "this command does not take arguments".
    InvalidNoArgs,
    /// Fall through to `Unknown` (the command line is treated as unknown).
    FallThroughUnknown,
}

/// A picker host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Host {
    Inline,
    Fullscreen,
    Pair,
}

/// One catalog spelling of a command, with its per-host presentation. `None` for
/// a host means the spelling is not shown in that host's picker. A spelling with
/// no host description is parse-only (a hidden alias).
#[derive(Debug, Clone, Copy)]
pub struct Spelling {
    pub command: SlashCommand,
    pub name: &'static str,
    pub args: ArgSpec,
    pub stray: StrayArgs,
    /// Typed metadata that a `compact` spelling forces compaction even within
    /// budget. `parse_slash` reads this instead of re-matching the spelling
    /// string, so the `compact_force`/`compact-force` distinction lives in the
    /// table, not in the parser.
    pub force: bool,
    pub inline: Option<&'static str>,
    pub fullscreen: Option<&'static str>,
    pub pair: Option<&'static str>,
}

impl Spelling {
    const fn inline_only(
        command: SlashCommand,
        name: &'static str,
        args: ArgSpec,
        stray: StrayArgs,
        desc: &'static str,
    ) -> Self {
        Self {
            command,
            name,
            args,
            stray,
            force: false,
            inline: Some(desc),
            fullscreen: None,
            pair: None,
        }
    }

    const fn both(
        command: SlashCommand,
        name: &'static str,
        args: ArgSpec,
        stray: StrayArgs,
        desc: &'static str,
    ) -> Self {
        Self {
            command,
            name,
            args,
            stray,
            force: false,
            inline: Some(desc),
            fullscreen: Some(desc),
            pair: None,
        }
    }

    const fn both_pair(
        command: SlashCommand,
        name: &'static str,
        args: ArgSpec,
        stray: StrayArgs,
        desc: &'static str,
    ) -> Self {
        Self {
            command,
            name,
            args,
            stray,
            force: false,
            inline: Some(desc),
            fullscreen: Some(desc),
            pair: Some(desc),
        }
    }

    /// A full-screen/pair takeover: no shared inline route yet, its own per-host
    /// copy, and its true argument syntax for the host that services it.
    const fn takeover(
        command: SlashCommand,
        name: &'static str,
        args: ArgSpec,
        stray: StrayArgs,
        fullscreen: &'static str,
        pair: &'static str,
    ) -> Self {
        Self {
            command,
            name,
            args,
            stray,
            force: false,
            inline: None,
            fullscreen: Some(fullscreen),
            pair: Some(pair),
        }
    }

    /// The permanent pair-only command. Its argument syntax describes the pair
    /// host that owns it; `parse_slash` never routes it.
    const fn pair_only(
        command: SlashCommand,
        name: &'static str,
        args: ArgSpec,
        stray: StrayArgs,
        desc: &'static str,
    ) -> Self {
        Self {
            command,
            name,
            args,
            stray,
            force: false,
            inline: None,
            fullscreen: None,
            pair: Some(desc),
        }
    }

    const fn parse_only(
        command: SlashCommand,
        name: &'static str,
        args: ArgSpec,
        stray: StrayArgs,
    ) -> Self {
        Self {
            command,
            name,
            args,
            stray,
            force: false,
            inline: None,
            fullscreen: None,
            pair: None,
        }
    }

    /// An inline-visible `compact` spelling that forces compaction.
    const fn inline_forcing(
        command: SlashCommand,
        name: &'static str,
        args: ArgSpec,
        stray: StrayArgs,
        desc: &'static str,
    ) -> Self {
        Self {
            command,
            name,
            args,
            stray,
            force: true,
            inline: Some(desc),
            fullscreen: None,
            pair: None,
        }
    }

    /// A hidden `compact` alias that forces compaction.
    const fn parse_forcing(
        command: SlashCommand,
        name: &'static str,
        args: ArgSpec,
        stray: StrayArgs,
    ) -> Self {
        Self {
            command,
            name,
            args,
            stray,
            force: true,
            inline: None,
            fullscreen: None,
            pair: None,
        }
    }

    const fn description_for(&self, host: Host) -> Option<&'static str> {
        match host {
            Host::Inline => self.inline,
            Host::Fullscreen => self.fullscreen,
            Host::Pair => self.pair,
        }
    }
}

use ArgSpec::{None as NoArg, Optional, Required};
use StrayArgs::{FallThroughUnknown as Fall, InvalidNoArgs as Reject};

/// Generate the [`SlashCommand`] enum, its [`SlashCommand::ALL`] identity list,
/// and the [`SLASH_SPELLINGS`] table from ONE definition. A spelling row names an
/// enum variant, so a row for a non-existent identity is a compile error; the
/// `commands`/`spellings` set equality is asserted by
/// `the_table_identities_equal_the_generated_command_set`, so a variant with no
/// row (or a row for no variant) cannot slip through.
macro_rules! slash_commands {
    (
        commands { $($variant:ident),+ $(,)? }
        spellings { $($id:ident => $ctor:ident ( $($arg:expr),* $(,)? ) ),+ $(,)? }
    ) => {
        /// The stable semantic identity of a command, independent of its
        /// spellings. One id may have several catalog spellings (`name`/`rename`,
        /// `continue`/`resume`, `compact`/`compact_force`, `exit`/`quit`) with
        /// different descriptions. Generated with [`SLASH_SPELLINGS`] and
        /// [`SlashCommand::ALL`] from one `slash_commands!` invocation.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum SlashCommand {
            $($variant),+
        }

        impl SlashCommand {
            /// Every command identity, generated from the same macro invocation
            /// as the enum so the two cannot drift.
            pub const ALL: &'static [SlashCommand] = &[$(SlashCommand::$variant),+];
        }

        /// The one authoritative, globally-ordered command+spelling catalog
        /// table. Every picker (`specs_for`), the name lookup, and the parser's
        /// dispatch derive from this single source; global source order is the
        /// presentation order.
        const SLASH_SPELLINGS: &[Spelling] = &[
            $( Spelling::$ctor(SlashCommand::$id, $($arg),*) ),+
        ];
    };
}

slash_commands! {
    commands {
        // Shared (inline + selectively full-screen) command identities.
        Agent, Harness, Default, Relaxed, Bypass, Unrestricted, Think, Effort,
        Model, Localbox, New, Fork, Clone, Tree, Sessions, Session, Name,
        Continue, Clear, Compact, HarnessResume, WaitResume, Ingest, Knowledge,
        Context, Research, Agents, Skills, Bg, Exit,
        // Full-screen/pair takeover identities. Their external string routing
        // (parse_slash -> Unknown) is temporary until a later change gives them
        // a shared route; their catalog scope stays full-screen/pair-only.
        Help, Theme, Settings, Diff, Search,
        // Permanent pair-only identity: `/abort` is owned by the pair event
        // loop, never parsed by `parse_slash`, present only in the pair picker.
        Abort,
    }
    spellings {
        // --- shared: 34 inline-visible rows, in the frozen order -------------
        Agent => inline_only("agent", NoArg, Reject, "Switch to agent mode"),
        Harness => inline_only("harness", NoArg, Reject, "Switch to harness mode"),
        Default => inline_only("default", NoArg, Reject, "Use the default permission profile"),
        Relaxed => inline_only("relaxed", NoArg, Reject, "Use the relaxed permission profile"),
        Bypass => inline_only("bypass", NoArg, Reject, "Use the bypass permission profile"),
        Unrestricted => inline_only(
            "unrestricted",
            NoArg,
            Reject,
            "Approve everything, workspace boundary included — you take responsibility"
        ),
        Think => inline_only("think", NoArg, Reject, "Toggle the reasoning panel"),
        Effort => inline_only("effort", Required, Fall, "Set reasoning effort: minimal|low|medium|high"),
        Model => both(
            "model",
            Optional,
            Fall,
            "Switch provider/model, or list them (/model [provider [model]])"
        ),
        Localbox => both(
            "localbox",
            Optional,
            Fall,
            "Adopt a running LocalBox server into your config (/localbox adopt)"
        ),
        New => both("new", NoArg, Fall, "Start a fresh session"),
        Fork => both("fork", NoArg, Fall, "Branch the conversation into a new session"),
        Clone => both("clone", NoArg, Fall, "Copy the conversation into a new session"),
        Tree => inline_only("tree", NoArg, Fall, "Show the session event tree"),
        Sessions => both("sessions", NoArg, Fall, "List this workspace's sessions"),
        Session => both("session", Required, Fall, "Resume a session by id"),
        Name => both("name", Required, Fall, "Name this session (/name <text>)"),
        Name => both("rename", Required, Fall, "Rename this session (/rename <text>)"),
        Continue => both("continue", Optional, Fall, "Continue the previous session"),
        Clear => both("clear", NoArg, Reject, "Clear the conversation view"),
        Compact => inline_only("compact", Optional, Fall, "Summarize and compact the context"),
        Compact => inline_forcing(
            "compact_force",
            NoArg,
            Reject,
            "Compact now, even if within the budget"
        ),
        Continue => both("resume", Optional, Fall, "Continue a previous session"),
        HarnessResume => inline_only("harness-resume", NoArg, Reject, "Resume harness plan work"),
        WaitResume => inline_only("wait-resume", NoArg, Reject, "Wait for quota, then resume"),
        Ingest => inline_only("ingest", Optional, Fall, "Manage workspace ingestion"),
        Knowledge => inline_only("knowledge", Required, Fall, "Query the knowledge base"),
        Context => inline_only("context", Required, Fall, "Build a context bundle"),
        Research => inline_only(
            "research",
            Optional,
            Fall,
            "Research a topic, local + web per config (/research [topic])"
        ),
        Agents => inline_only(
            "agents",
            Optional,
            Fall,
            "List or inspect subagent definitions (/agents [show <name>])"
        ),
        // `/skills` opens the skills surface with no subcommand (`Skills("")`),
        // so it is Optional, not Required.
        Skills => inline_only(
            "skills",
            Optional,
            Fall,
            "Manage skills: repos, install, list (/skills <subcommand>)"
        ),
        Bg => inline_only("bg", Optional, Fall, "List background processes (/bg stop <id>|all)"),
        Exit => both_pair("exit", Optional, Reject, "Exit LocalPilot (/exit [print])"),
        // `/quit` accepts the same optional `print` argument as `/exit`.
        Exit => both_pair("quit", Optional, Reject, "Exit LocalPilot"),
        // --- full-screen/pair takeovers (per-host copy; not inline) -----------
        // The inline host routes all five to `Unknown` — a host gate outside the
        // builder, not a metadata quirk. For full-screen/pair they route to real
        // actions. The `ArgSpec`/`StrayArgs` metadata stays host-independent and
        // truthful: `help` is `NoArg`/`Reject` (a stray argument is `Invalid`
        // "this command does not take arguments", via the table-driven `no_arg`
        // path), while `theme`/`settings`/`diff`/`search` are `Optional` (any
        // trailing text is the name/query/path, so `stray` never applies).
        Search => takeover(
            "search",
            // `/search` accepts an optional query (bare `/search` opens search;
            // `/search <query>` seeds it), so its owning-host syntax is Optional.
            Optional,
            Fall,
            "Search messages in this session",
            "Search messages for the selected peer"
        ),
        Help => takeover(
            "help",
            NoArg,
            Reject,
            "Open keyboard and command help",
            "Open keyboard and command help"
        ),
        Theme => takeover(
            "theme",
            // `/theme` opens the picker; `/theme <name>` applies a theme directly.
            Optional,
            Fall,
            "Preview terminal color modes",
            "Preview terminal color modes"
        ),
        Settings => takeover(
            "settings",
            // `/settings` opens settings; `/settings <query>` pre-fills the filter.
            Optional,
            Fall,
            "Inspect terminal chat settings",
            "Inspect terminal settings"
        ),
        Diff => takeover(
            "diff",
            // `/diff` shows all changes; `/diff <path>` filters by path substring.
            Optional,
            Fall,
            "Review tracked workspace changes",
            "Review tracked workspace changes"
        ),
        // --- permanent pair-only, pair-loop-owned: no-arg, rejects a stray ----
        Abort => pair_only("abort", NoArg, Reject, "Stop the collaboration and both peers"),
        // --- parse-only aliases (hidden; present for lookup + stray policy) ---
        Think => parse_only("thinking", NoArg, Reject),
        Compact => parse_forcing("compact-force", NoArg, Reject),
        WaitResume => parse_only("wait_resume", NoArg, Reject),
        Exit => parse_only("q", Optional, Reject),
    }
}

use SlashCommand as C;

/// The catalog rows for one host, in global order: each spelling with a
/// description for that host, as `(name, description)`.
#[must_use]
pub fn specs_for(host: Host) -> Vec<(&'static str, &'static str)> {
    SLASH_SPELLINGS
        .iter()
        .filter_map(|spelling| {
            spelling
                .description_for(host)
                .map(|desc| (spelling.name, desc))
        })
        .collect()
}

/// The spelling matching an exact command name (any spelling, including hidden
/// parse-only aliases), or `None`.
#[must_use]
pub fn lookup(name: &str) -> Option<&'static Spelling> {
    SLASH_SPELLINGS
        .iter()
        .find(|spelling| spelling.name == name)
}

impl SlashAction {
    /// The semantic command identity this action came from, or `None` for the
    /// non-command results (`Invalid`, `Unknown`) and the mode-only
    /// `SetMode(Research)` (which no slash command produces).
    #[must_use]
    pub fn command(&self) -> Option<SlashCommand> {
        Some(match self {
            SlashAction::SetMode(Mode::Agent) => C::Agent,
            SlashAction::SetMode(Mode::Harness) => C::Harness,
            SlashAction::SetMode(Mode::Research) => return None,
            SlashAction::SetProfile(Profile::Default) => C::Default,
            SlashAction::SetProfile(Profile::Relaxed) => C::Relaxed,
            SlashAction::SetProfile(Profile::Bypass) => C::Bypass,
            SlashAction::SetProfile(Profile::Unrestricted) => C::Unrestricted,
            SlashAction::ToggleThinking => C::Think,
            SlashAction::SetEffort(_) => C::Effort,
            SlashAction::Model { .. } => C::Model,
            SlashAction::LocalBoxAdopt => C::Localbox,
            SlashAction::NewSession => C::New,
            SlashAction::Fork => C::Fork,
            SlashAction::CloneSession => C::Clone,
            SlashAction::Tree => C::Tree,
            SlashAction::Sessions => C::Sessions,
            SlashAction::LoadSession(_) => C::Session,
            SlashAction::NameSession(_) => C::Name,
            SlashAction::ContinueSession(_) => C::Continue,
            SlashAction::Clear => C::Clear,
            SlashAction::Compact { .. } => C::Compact,
            SlashAction::HarnessResume => C::HarnessResume,
            SlashAction::WaitResume => C::WaitResume,
            SlashAction::Ingest(_) => C::Ingest,
            SlashAction::Knowledge(_) => C::Knowledge,
            SlashAction::ContextBuild(_) => C::Context,
            SlashAction::Research(_) => C::Research,
            SlashAction::Agents(_) => C::Agents,
            SlashAction::Skills(_) => C::Skills,
            SlashAction::Background(_) => C::Bg,
            SlashAction::Exit { .. } => C::Exit,
            SlashAction::Help => C::Help,
            SlashAction::Theme(_) => C::Theme,
            SlashAction::Settings(_) => C::Settings,
            SlashAction::Diff(_) => C::Diff,
            SlashAction::Search(_) => C::Search,
            SlashAction::Invalid { .. } | SlashAction::Unknown(_) => return None,
        })
    }
}

/// Parse a slash command from an input line.
///
/// Lookup-first: the name is resolved to its [`Spelling`] once, then dispatched
/// on the spelling's typed [`SlashCommand`] through one wildcard-free match. An
/// unknown name is `Unknown`; the five takeovers and the pair-only `abort` have
/// no shared route and resolve to `Unknown` exactly as before this table existed.
#[must_use]
pub fn parse_slash(line: &str) -> Option<SlashAction> {
    parse_slash_for(Host::Inline, line)
}

/// Host-aware parse. Identical to [`parse_slash`] for `Host::Inline`; for
/// `Host::Fullscreen`/`Host::Pair` the five takeover spellings resolve to their
/// real actions ([`SlashAction::Help`]/`Theme`/`Settings`/`Diff`/`Search`)
/// instead of `Unknown`. The pair-only `abort` stays `Unknown` for every host —
/// the pair loop owns it as an exact-token route ahead of the parser.
#[must_use]
pub fn parse_slash_for(host: Host, line: &str) -> Option<SlashAction> {
    let command = line.trim().strip_prefix('/')?.trim();
    let (name, args) = command
        .split_once(char::is_whitespace)
        .map_or((command, ""), |(name, args)| (name, args.trim()));
    let Some(spelling) = lookup(name) else {
        return Some(SlashAction::Unknown(command.to_string()));
    };
    Some(dispatch(spelling, host, name, args, command))
}

/// Dispatch a resolved spelling to its action. One arm per [`SlashCommand`]
/// identity — no wildcard — so a new identity is a compile error here, never a
/// silent fall-through. Complex payload/subcommand parsing stays inside the
/// relevant semantic arms; simple no-argument commands defer to [`no_arg`], and
/// the frozen stray-argument policy lives in [`stray`].
fn dispatch(spelling: &Spelling, host: Host, name: &str, args: &str, command: &str) -> SlashAction {
    match spelling.command {
        C::Agent => no_arg(
            spelling,
            name,
            args,
            command,
            SlashAction::SetMode(Mode::Agent),
        ),
        C::Harness => no_arg(
            spelling,
            name,
            args,
            command,
            SlashAction::SetMode(Mode::Harness),
        ),
        C::Default => no_arg(
            spelling,
            name,
            args,
            command,
            SlashAction::SetProfile(Profile::Default),
        ),
        C::Relaxed => no_arg(
            spelling,
            name,
            args,
            command,
            SlashAction::SetProfile(Profile::Relaxed),
        ),
        C::Bypass => no_arg(
            spelling,
            name,
            args,
            command,
            SlashAction::SetProfile(Profile::Bypass),
        ),
        C::Unrestricted => no_arg(
            spelling,
            name,
            args,
            command,
            SlashAction::SetProfile(Profile::Unrestricted),
        ),
        C::Think => no_arg(spelling, name, args, command, SlashAction::ToggleThinking),
        C::Effort => {
            if args.is_empty() {
                SlashAction::Invalid {
                    command: name.to_string(),
                    reason: "usage: /effort minimal|low|medium|high".to_string(),
                }
            } else {
                SlashAction::SetEffort(args.to_string())
            }
        }
        C::Model => {
            // `/model` lists; `/model <provider>` switches to that provider's
            // default model; `/model <provider> <model>` switches both.
            let (provider, model) = args
                .split_once(char::is_whitespace)
                .map_or((args, ""), |(provider, model)| (provider, model.trim()));
            SlashAction::Model {
                provider: (!provider.is_empty()).then(|| provider.to_string()),
                model: (!model.is_empty()).then(|| model.to_string()),
            }
        }
        C::Localbox => {
            if args.is_empty() || args == "adopt" {
                SlashAction::LocalBoxAdopt
            } else {
                SlashAction::Invalid {
                    command: name.to_string(),
                    reason: "usage: /localbox adopt — add a running LocalBox server to your config"
                        .to_string(),
                }
            }
        }
        C::New => no_arg(spelling, name, args, command, SlashAction::NewSession),
        C::Fork => no_arg(spelling, name, args, command, SlashAction::Fork),
        C::Clone => no_arg(spelling, name, args, command, SlashAction::CloneSession),
        C::Tree => no_arg(spelling, name, args, command, SlashAction::Tree),
        C::Sessions => no_arg(spelling, name, args, command, SlashAction::Sessions),
        C::Session => {
            if args.is_empty() {
                SlashAction::Invalid {
                    command: name.to_string(),
                    reason: "usage: /session <id> (see /sessions)".to_string(),
                }
            } else {
                SlashAction::LoadSession(args.to_string())
            }
        }
        C::Name => {
            if args.is_empty() {
                SlashAction::Invalid {
                    command: name.to_string(),
                    reason: "usage: /name <text> — a name for this conversation".to_string(),
                }
            } else {
                SlashAction::NameSession(args.to_string())
            }
        }
        C::Continue => {
            let id = (!args.is_empty()).then(|| args.to_string());
            SlashAction::ContinueSession(id)
        }
        C::Clear => no_arg(spelling, name, args, command, SlashAction::Clear),
        C::Compact => {
            if spelling.force {
                // `compact_force` / `compact-force`: force, no argument.
                no_arg(
                    spelling,
                    name,
                    args,
                    command,
                    SlashAction::Compact { force: true },
                )
            } else {
                match args {
                    "" => SlashAction::Compact { force: false },
                    "force" => SlashAction::Compact { force: true },
                    _ => SlashAction::Invalid {
                        command: name.to_string(),
                        reason: "usage: /compact [force]".to_string(),
                    },
                }
            }
        }
        C::HarnessResume => no_arg(spelling, name, args, command, SlashAction::HarnessResume),
        C::WaitResume => no_arg(spelling, name, args, command, SlashAction::WaitResume),
        C::Ingest => parse_ingest(args),
        C::Knowledge => {
            if args.is_empty() {
                SlashAction::Invalid {
                    command: name.to_string(),
                    reason: "usage: /knowledge <query>".to_string(),
                }
            } else {
                SlashAction::Knowledge(args.to_string())
            }
        }
        C::Context => parse_context(args),
        C::Research => {
            if args.is_empty() {
                SlashAction::Research(None)
            } else {
                SlashAction::Research(Some(args.to_string()))
            }
        }
        // `/agents …` / `/skills …` capture their raw arguments; the host parses
        // them with the same command surface as the CLI for exact parity.
        C::Agents => SlashAction::Agents(args.to_string()),
        C::Skills => SlashAction::Skills(args.to_string()),
        C::Bg => parse_bg(args),
        C::Exit => match args {
            "" | "print" => SlashAction::Exit {
                print_transcript: args == "print",
            },
            _ => stray(spelling, name, command),
        },
        // The pair-only `abort` has no shared parse route on any host — the pair
        // event loop owns it as an exact-token route ahead of the parser.
        C::Abort => SlashAction::Unknown(command.to_string()),
        // The five full-screen/pair takeovers resolve to `Unknown` for the
        // inline host (which never routes them) and to their real actions for
        // the full-screen and pair hosts.
        // `help` is no-arg: the table-driven `no_arg`/`stray` path yields `Help`
        // for a bare form and the exact `Invalid` reason for a stray argument, so
        // the no-arg policy is not duplicated here.
        C::Help => routed_takeover(host, command, || {
            no_arg(spelling, name, args, command, SlashAction::Help)
        }),
        C::Theme => routed_takeover(host, command, || SlashAction::Theme(opt_arg(args))),
        C::Settings => routed_takeover(host, command, || SlashAction::Settings(opt_arg(args))),
        C::Diff => routed_takeover(host, command, || SlashAction::Diff(opt_arg(args))),
        C::Search => routed_takeover(host, command, || SlashAction::Search(opt_arg(args))),
    }
}

/// A trimmed argument tail as an `Option`: `None` when empty.
fn opt_arg(args: &str) -> Option<String> {
    (!args.is_empty()).then(|| args.to_string())
}

/// Resolve a full-screen/pair takeover: `Unknown` for the inline rollback host
/// (which never routes these), the built action for full-screen and pair.
fn routed_takeover(host: Host, command: &str, build: impl FnOnce() -> SlashAction) -> SlashAction {
    match host {
        Host::Inline => SlashAction::Unknown(command.to_string()),
        Host::Fullscreen | Host::Pair => build(),
    }
}

/// A no-argument command: the action when no argument follows, else the frozen
/// stray-argument policy for this spelling.
fn no_arg(
    spelling: &Spelling,
    name: &str,
    args: &str,
    command: &str,
    action: SlashAction,
) -> SlashAction {
    if args.is_empty() {
        action
    } else {
        stray(spelling, name, command)
    }
}

/// The frozen stray-argument policy: a `InvalidNoArgs` spelling rejects with
/// "does not take arguments"; a `FallThroughUnknown` spelling is treated as an
/// unknown command line, exactly as before this table existed.
fn stray(spelling: &Spelling, name: &str, command: &str) -> SlashAction {
    match spelling.stray {
        StrayArgs::InvalidNoArgs => SlashAction::Invalid {
            command: name.to_string(),
            reason: "this command does not take arguments".to_string(),
        },
        StrayArgs::FallThroughUnknown => SlashAction::Unknown(command.to_string()),
    }
}

fn parse_ingest(args: &str) -> SlashAction {
    if args.is_empty() {
        return SlashAction::Ingest(IngestAction::Run);
    }
    let (subcommand, rest) = args
        .split_once(char::is_whitespace)
        .map_or((args, ""), |(name, rest)| (name, rest.trim()));
    match subcommand {
        "preview" if rest.is_empty() => SlashAction::Ingest(IngestAction::Preview),
        "status" if rest.is_empty() => SlashAction::Ingest(IngestAction::Status),
        "pause" if rest.is_empty() => SlashAction::Ingest(IngestAction::Pause),
        "resume" if rest.is_empty() => SlashAction::Ingest(IngestAction::Resume),
        "cancel" if rest.is_empty() => SlashAction::Ingest(IngestAction::Cancel),
        "refresh" if rest.is_empty() => SlashAction::Ingest(IngestAction::Refresh),
        "rebuild" if rest.is_empty() => SlashAction::Ingest(IngestAction::Rebuild),
        "skipped" if rest.is_empty() => SlashAction::Ingest(IngestAction::Skipped),
        "include" if !rest.is_empty() => {
            SlashAction::Ingest(IngestAction::Include(rest.to_string()))
        }
        "exclude" if !rest.is_empty() => {
            SlashAction::Ingest(IngestAction::Exclude(rest.to_string()))
        }
        "forget" if !rest.is_empty() => {
            SlashAction::Ingest(IngestAction::Forget(rest.to_string()))
        }
        "review" if rest.is_empty() => SlashAction::Ingest(IngestAction::Review),
        "promote" if !rest.is_empty() => {
            SlashAction::Ingest(IngestAction::Promote(rest.to_string()))
        }
        _ => SlashAction::Invalid {
            command: "ingest".to_string(),
            reason: "usage: /ingest [preview|status|pause|resume|cancel|refresh|rebuild|skipped|include <path>|exclude <path>|forget <path-or-id>|review|promote <id>]".to_string(),
        },
    }
}

fn parse_bg(args: &str) -> SlashAction {
    if args.is_empty() {
        return SlashAction::Background(BackgroundCommand::List);
    }
    let (subcommand, rest) = args
        .split_once(char::is_whitespace)
        .map_or((args, ""), |(name, rest)| (name, rest.trim()));
    match subcommand {
        "list" if rest.is_empty() => SlashAction::Background(BackgroundCommand::List),
        "stop" if rest == "all" => SlashAction::Background(BackgroundCommand::StopAll),
        "stop" if !rest.is_empty() => {
            SlashAction::Background(BackgroundCommand::Stop(rest.to_string()))
        }
        _ => SlashAction::Invalid {
            command: "bg".to_string(),
            reason: "usage: /bg [list | stop <id> | stop all]".to_string(),
        },
    }
}

fn parse_context(args: &str) -> SlashAction {
    let (subcommand, rest) = args
        .split_once(char::is_whitespace)
        .map_or((args, ""), |(name, rest)| (name, rest.trim()));
    if subcommand == "build" && !rest.is_empty() {
        SlashAction::ContextBuild(rest.to_string())
    } else {
        SlashAction::Invalid {
            command: "context".to_string(),
            reason: "usage: /context build <task>".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeSet, HashSet};

    #[test]
    fn every_command_name_and_alias_is_globally_unique() {
        let mut names = BTreeSet::new();
        for spelling in SLASH_SPELLINGS {
            assert!(
                names.insert(spelling.name),
                "duplicate spelling: {}",
                spelling.name
            );
        }
    }

    #[test]
    fn the_table_identities_equal_the_generated_command_set() {
        // Structural anti-drift: the identities used by the table must be
        // exactly the identities generated into `SlashCommand::ALL`. A variant
        // added without a spelling row (or a row for no variant) breaks this —
        // no count that can't see an omitted variant.
        let from_table: HashSet<SlashCommand> = SLASH_SPELLINGS
            .iter()
            .map(|spelling| spelling.command)
            .collect();
        let from_enum: HashSet<SlashCommand> = SlashCommand::ALL.iter().copied().collect();
        assert_eq!(
            from_table, from_enum,
            "SLASH_SPELLINGS identities must equal SlashCommand::ALL"
        );
        assert_eq!(from_enum.len(), 36, "expected 36 command identities");
    }

    #[test]
    fn catalogs_have_the_frozen_cardinalities() {
        assert_eq!(specs_for(Host::Inline).len(), 34);
        assert_eq!(specs_for(Host::Fullscreen).len(), 19);
        assert_eq!(specs_for(Host::Pair).len(), 8);
    }

    #[test]
    fn inline_catalog_matches_the_frozen_golden() {
        // The inline picker is the 34-row shared surface, byte-for-byte, in the
        // frozen order. The takeovers never join it (inline stays 34).
        let expected: &[(&str, &str)] = &[
            ("agent", "Switch to agent mode"),
            ("harness", "Switch to harness mode"),
            ("default", "Use the default permission profile"),
            ("relaxed", "Use the relaxed permission profile"),
            ("bypass", "Use the bypass permission profile"),
            (
                "unrestricted",
                "Approve everything, workspace boundary included — you take responsibility",
            ),
            ("think", "Toggle the reasoning panel"),
            ("effort", "Set reasoning effort: minimal|low|medium|high"),
            (
                "model",
                "Switch provider/model, or list them (/model [provider [model]])",
            ),
            (
                "localbox",
                "Adopt a running LocalBox server into your config (/localbox adopt)",
            ),
            ("new", "Start a fresh session"),
            ("fork", "Branch the conversation into a new session"),
            ("clone", "Copy the conversation into a new session"),
            ("tree", "Show the session event tree"),
            ("sessions", "List this workspace's sessions"),
            ("session", "Resume a session by id"),
            ("name", "Name this session (/name <text>)"),
            ("rename", "Rename this session (/rename <text>)"),
            ("continue", "Continue the previous session"),
            ("clear", "Clear the conversation view"),
            ("compact", "Summarize and compact the context"),
            ("compact_force", "Compact now, even if within the budget"),
            ("resume", "Continue a previous session"),
            ("harness-resume", "Resume harness plan work"),
            ("wait-resume", "Wait for quota, then resume"),
            ("ingest", "Manage workspace ingestion"),
            ("knowledge", "Query the knowledge base"),
            ("context", "Build a context bundle"),
            (
                "research",
                "Research a topic, local + web per config (/research [topic])",
            ),
            (
                "agents",
                "List or inspect subagent definitions (/agents [show <name>])",
            ),
            (
                "skills",
                "Manage skills: repos, install, list (/skills <subcommand>)",
            ),
            ("bg", "List background processes (/bg stop <id>|all)"),
            ("exit", "Exit LocalPilot (/exit [print])"),
            ("quit", "Exit LocalPilot"),
        ];
        let inline = specs_for(Host::Inline);
        assert_eq!(inline.len(), expected.len());
        for (got, want) in inline.iter().zip(expected.iter()) {
            assert_eq!(*got, *want);
        }
    }

    #[test]
    fn full_screen_catalog_lists_no_inline_only_deferred_row() {
        let full_screen: BTreeSet<_> = specs_for(Host::Fullscreen)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        // A deferred inline-only name must not appear in the full-screen picker.
        for deferred in [
            "agent", "harness", "compact", "research", "skills", "bg", "tree",
        ] {
            assert!(
                !full_screen.contains(deferred),
                "{deferred} leaked into full-screen"
            );
        }
    }

    #[test]
    fn arg_specs_and_stray_are_truthful() {
        let args = |name: &str| lookup(name).map(|spelling| spelling.args);
        let stray = |name: &str| lookup(name).map(|spelling| spelling.stray);

        // Required-argument commands.
        for name in [
            "effort",
            "session",
            "name",
            "rename",
            "knowledge",
            "context",
        ] {
            assert_eq!(args(name), Some(ArgSpec::Required), "{name} is Required");
        }
        // Optional-argument commands (a bare form is valid). Includes the
        // `search`/`theme`/`settings`/`diff` takeovers, which accept an optional
        // query/name/path in their owning host.
        for name in [
            "model", "localbox", "continue", "resume", "compact", "research", "ingest", "bg",
            "agents", "skills", "exit", "quit", "q", "search", "theme", "settings", "diff",
        ] {
            assert_eq!(args(name), Some(ArgSpec::Optional), "{name} is Optional");
        }
        // No-argument commands: the `help` takeover and the pair-only `abort`
        // (the other four takeovers are Optional, above).
        for name in [
            "agent",
            "harness",
            "default",
            "relaxed",
            "bypass",
            "unrestricted",
            "think",
            "thinking",
            "new",
            "fork",
            "clone",
            "tree",
            "sessions",
            "clear",
            "compact_force",
            "compact-force",
            "harness-resume",
            "wait-resume",
            "wait_resume",
            "help",
            "abort",
        ] {
            assert_eq!(args(name), Some(ArgSpec::None), "{name} is no-arg");
        }
        // The inline host never routes the takeovers: both forms stay `Unknown`.
        assert_eq!(
            parse_slash("/search"),
            Some(SlashAction::Unknown("search".to_string()))
        );
        assert_eq!(
            parse_slash("/search foo"),
            Some(SlashAction::Unknown("search foo".to_string()))
        );

        // Representative parse forms exercising the corrected metadata.
        assert_eq!(
            parse_slash("/skills"),
            Some(SlashAction::Skills(String::new())),
            "/skills is Optional -> Skills(\"\")",
        );
        assert_eq!(
            parse_slash("/quit print"),
            Some(SlashAction::Exit {
                print_transcript: true
            }),
            "/quit accepts optional print like /exit",
        );
        // The pair host that owns `abort` rejects a stray argument.
        assert_eq!(stray("abort"), Some(StrayArgs::InvalidNoArgs));
        // `help` is a bare takeover: no-arg metadata AND a truthful stray-reject
        // policy (host-independent), so `/help me` is `Invalid`, never `Unknown`.
        assert_eq!(args("help"), Some(ArgSpec::None));
        assert_eq!(stray("help"), Some(StrayArgs::InvalidNoArgs));
    }

    #[test]
    fn abort_is_pair_only_and_never_parsed_or_bridged() {
        // `abort` is external for every host, including the pair host that owns
        // it as an exact-token route ahead of the parser.
        for host in [Host::Inline, Host::Fullscreen, Host::Pair] {
            assert_eq!(
                parse_slash_for(host, "/abort"),
                Some(SlashAction::Unknown("abort".to_string())),
                "abort must stay external for {host:?}",
            );
        }
        let pair: BTreeSet<_> = specs_for(Host::Pair).into_iter().map(|(n, _)| n).collect();
        assert!(pair.contains("abort"));
        assert!(!specs_for(Host::Inline).iter().any(|(n, _)| *n == "abort"));
        assert!(!specs_for(Host::Fullscreen)
            .iter()
            .any(|(n, _)| *n == "abort"));
    }

    #[test]
    fn takeovers_are_unknown_for_the_inline_host() {
        // The inline rollback host never routes the five takeovers: bare and
        // with-argument forms both resolve to `Unknown`, never a semantic action.
        for name in ["search", "help", "theme", "settings", "diff"] {
            assert_eq!(
                parse_slash(&format!("/{name}")),
                Some(SlashAction::Unknown(name.to_string())),
            );
            assert_eq!(
                parse_slash(&format!("/{name} x")),
                Some(SlashAction::Unknown(format!("{name} x"))),
            );
        }
    }

    #[test]
    fn takeovers_route_for_fullscreen_and_pair() {
        for host in [Host::Fullscreen, Host::Pair] {
            assert_eq!(parse_slash_for(host, "/help"), Some(SlashAction::Help));
            // `/help me` is the exact table-driven no-arg rejection, never
            // `Unknown` — the same reason the other guarded no-arg commands give.
            assert_eq!(
                parse_slash_for(host, "/help me"),
                Some(SlashAction::Invalid {
                    command: "help".to_string(),
                    reason: "this command does not take arguments".to_string(),
                })
            );
            assert_eq!(
                parse_slash_for(host, "/theme"),
                Some(SlashAction::Theme(None))
            );
            assert_eq!(
                parse_slash_for(host, "/theme dim"),
                Some(SlashAction::Theme(Some("dim".to_string())))
            );
            assert_eq!(
                parse_slash_for(host, "/settings"),
                Some(SlashAction::Settings(None))
            );
            assert_eq!(
                parse_slash_for(host, "/settings mouse"),
                Some(SlashAction::Settings(Some("mouse".to_string())))
            );
            assert_eq!(
                parse_slash_for(host, "/diff"),
                Some(SlashAction::Diff(None))
            );
            assert_eq!(
                parse_slash_for(host, "/diff src"),
                Some(SlashAction::Diff(Some("src".to_string())))
            );
            assert_eq!(
                parse_slash_for(host, "/search"),
                Some(SlashAction::Search(None))
            );
            assert_eq!(
                parse_slash_for(host, "/search foo"),
                Some(SlashAction::Search(Some("foo".to_string())))
            );
            // None of the five ever resolves to `Unknown` in these hosts.
            for name in ["help", "theme", "settings", "diff", "search"] {
                assert!(
                    !matches!(
                        parse_slash_for(host, &format!("/{name}")),
                        Some(SlashAction::Unknown(_))
                    ),
                    "/{name} must not be Unknown for {host:?}",
                );
            }
        }
    }

    #[test]
    fn command_round_trips_over_takeovers() {
        assert_eq!(SlashAction::Help.command(), Some(C::Help));
        assert_eq!(SlashAction::Theme(None).command(), Some(C::Theme));
        assert_eq!(
            SlashAction::Theme(Some("dim".to_string())).command(),
            Some(C::Theme)
        );
        assert_eq!(SlashAction::Settings(None).command(), Some(C::Settings));
        assert_eq!(SlashAction::Diff(None).command(), Some(C::Diff));
        assert_eq!(SlashAction::Search(None).command(), Some(C::Search));
    }

    #[test]
    fn every_semantic_name_parses_to_its_command_id() {
        for spelling in SLASH_SPELLINGS {
            // The pair-only Abort has no `SlashAction` variant — it is owned by
            // the pair loop as an exact-token route, never bridged.
            if matches!(spelling.command, C::Abort) {
                continue;
            }
            let line = match (spelling.command, spelling.args) {
                // `/context` requires the `build <task>` subcommand form.
                (C::Context, _) => "/context build a task".to_string(),
                (_, ArgSpec::None | ArgSpec::Optional) => format!("/{}", spelling.name),
                (_, ArgSpec::Required) => format!("/{} x", spelling.name),
            };
            // The five takeovers route only for full-screen/pair; every other
            // command parses identically on every host, so the full-screen host
            // exercises them all.
            let action = parse_slash_for(Host::Fullscreen, &line)
                .unwrap_or_else(|| panic!("{line} did not parse"));
            assert_eq!(
                action.command(),
                Some(spelling.command),
                "{line} -> {action:?}",
            );
        }
    }

    #[test]
    fn no_arg_stray_policy_matches_the_frozen_parser() {
        // The 17 guarded spellings reject stray arguments.
        for name in [
            "agent",
            "harness",
            "default",
            "relaxed",
            "bypass",
            "unrestricted",
            "think",
            "thinking",
            "clear",
            "compact_force",
            "compact-force",
            "harness-resume",
            "wait-resume",
            "wait_resume",
            "exit",
            "quit",
            "q",
        ] {
            assert!(
                matches!(
                    parse_slash(&format!("/{name} stray")),
                    Some(SlashAction::Invalid { .. })
                ),
                "/{name} stray should be Invalid",
            );
        }
        // The five no-arg commands omitted from the guard fall through to Unknown.
        for name in ["new", "fork", "clone", "tree", "sessions"] {
            assert!(
                matches!(
                    parse_slash(&format!("/{name} stray")),
                    Some(SlashAction::Unknown(_))
                ),
                "/{name} stray should be Unknown",
            );
        }
    }

    #[test]
    fn alias_sensitive_forms_parse_exactly() {
        assert_eq!(
            parse_slash("/compact"),
            Some(SlashAction::Compact { force: false })
        );
        assert_eq!(
            parse_slash("/compact force"),
            Some(SlashAction::Compact { force: true })
        );
        assert_eq!(
            parse_slash("/compact_force"),
            Some(SlashAction::Compact { force: true })
        );
        assert_eq!(
            parse_slash("/compact-force"),
            Some(SlashAction::Compact { force: true })
        );
        assert!(matches!(
            parse_slash("/compact bogus"),
            Some(SlashAction::Invalid { .. })
        ));
        assert_eq!(
            parse_slash("/exit"),
            Some(SlashAction::Exit {
                print_transcript: false
            })
        );
        assert_eq!(
            parse_slash("/exit print"),
            Some(SlashAction::Exit {
                print_transcript: true
            })
        );
        assert_eq!(
            parse_slash("/quit"),
            Some(SlashAction::Exit {
                print_transcript: false
            })
        );
        assert_eq!(
            parse_slash("/q print"),
            Some(SlashAction::Exit {
                print_transcript: true
            })
        );
        assert_eq!(
            parse_slash("/name a"),
            Some(SlashAction::NameSession("a".to_string())),
        );
        assert_eq!(
            parse_slash("/rename a"),
            Some(SlashAction::NameSession("a".to_string())),
        );
        assert_eq!(
            parse_slash("/continue"),
            Some(SlashAction::ContinueSession(None)),
        );
        assert_eq!(
            parse_slash("/resume x"),
            Some(SlashAction::ContinueSession(Some("x".to_string()))),
        );
        assert_eq!(parse_slash("/think"), Some(SlashAction::ToggleThinking));
        assert_eq!(parse_slash("/wait-resume"), Some(SlashAction::WaitResume));
        assert_eq!(parse_slash("/wait_resume"), Some(SlashAction::WaitResume));
    }
}
