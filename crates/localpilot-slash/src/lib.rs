//! The shared interactive slash-command surface.
//!
//! One dependency-free source of command knowledge for every interactive host:
//! the parsed action types, the parser, and one authoritative globally-ordered
//! command+spelling catalog table that generates the full-screen and pair
//! pickers. Both hosts consume these types so command names, descriptions,
//! order, and argument policy cannot drift between the full-screen and pair UIs.
//!
//! The command enum, the catalog table, and the identity list are generated from
//! one [`slash_commands!`] invocation, so a new command identity cannot exist
//! without a catalog row (and vice versa), and [`parse_slash_for`] dispatches on
//! the typed [`SlashCommand`] of the looked-up spelling — never on a raw string.

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

/// One explicit action in the persisted self-improvement loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelfImproveAction {
    /// Show the current persisted stage (bare `/selfimprove` is equivalent).
    Status,
    /// Review and propose one finding. `finding` is a 1-based rank; when omitted,
    /// a single finding is selected automatically and multiple findings are listed.
    Start { finding: Option<usize> },
    /// Advance exactly one non-approval stage, stopping at the human gate.
    Next,
    /// Cross the human gate for the named reviewer.
    Approve { reviewer: String },
    /// Clear the persisted loop state.
    Reset,
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
    /// Adopt a LocalBox server into `.localpilot.toml` from inside the session.
    /// `serve` starts that model first when no server is already running
    /// (`/localbox adopt --serve <model>`); bare `/localbox` and
    /// `/localbox adopt` retain their running-server behavior.
    LocalBoxAdopt {
        serve: Option<String>,
    },
    /// List the LocalBox-owned launch catalog and run-profile state.
    LocalBoxModels,
    /// Start a named LocalBox model, then adopt and switch this session.
    LocalBoxServe {
        model: String,
        /// Explicit one-shot approval to use defaults when no tuned profile exists.
        allow_untuned: bool,
    },
    /// Inspect or advance the persisted, human-gated self-improvement loop.
    SelfImprove(SelfImproveAction),
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
    /// Activate the six-section LocalMind workspace tab. Full-screen only; no arguments.
    LocalMind,
    /// Toggle incognito mode: `/incognito` starts a non-persistent session (a
    /// fresh session that saves nothing and gates every file it creates);
    /// `/incognito off` ends it and reports what was created. Full-screen only.
    Incognito {
        off: bool,
    },
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

/// Execution lane of an ingest subcommand. `LongRunning` actions walk the
/// workspace under a spinner (an async, pumped path); `Fast` actions return
/// promptly and can be presented directly. This is the production dispatch
/// authority — hosts route on it, so a new variant is a compile error here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestTier {
    Fast,
    LongRunning,
}

impl IngestAction {
    /// Classify this action's execution lane. Wildcard-free so a new
    /// `IngestAction` variant cannot be silently routed through the wrong lane.
    #[must_use]
    pub const fn tier(&self) -> IngestTier {
        match self {
            IngestAction::Run | IngestAction::Refresh | IngestAction::Resume => {
                IngestTier::LongRunning
            }
            IngestAction::Preview
            | IngestAction::Status
            | IngestAction::Pause
            | IngestAction::Cancel
            | IngestAction::Rebuild
            | IngestAction::Skipped
            | IngestAction::Include(_)
            | IngestAction::Exclude(_)
            | IngestAction::Forget(_)
            | IngestAction::Review
            | IngestAction::Promote(_) => IngestTier::Fast,
        }
    }
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
    Fullscreen,
    Pair,
}

/// Whether a slash action leaves anything on disk — the axis an incognito
/// session gates on. Kept exhaustive (wildcard-free) so a new [`SlashAction`]
/// must be classified before it compiles, and a persistent action can never be
/// added that an incognito session then silently runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Persistence {
    /// Reads or shows state; writes nothing.
    ReadOnly,
    /// Touches only the session store, which is in-memory under incognito, so it
    /// leaves nothing on disk there.
    MemoryOnly,
    /// Writes to disk, or starts a durable service, that outlives the session.
    /// The string names what would be written, for an incognito refusal message.
    Persistent(&'static str),
}

impl Persistence {
    /// What this persistent action would write, or `None` when it writes
    /// nothing durable.
    #[must_use]
    pub fn persistent_target(self) -> Option<&'static str> {
        match self {
            Persistence::Persistent(what) => Some(what),
            Persistence::ReadOnly | Persistence::MemoryOnly => None,
        }
    }
}

/// Whether a raw `/skills` argument names a writing subcommand. `/skills` bare
/// (or `list`/`show`/`research` read paths) inspects only; `install`, `add`,
/// `remove`, `delete`, `update`, `repo`, and `sync` change what is on disk.
#[must_use]
fn skills_arg_writes(arg: &str) -> Option<&'static str> {
    match arg.split_whitespace().next().unwrap_or("") {
        "install" | "add" | "update" | "sync" => Some("install or update a skill on disk"),
        "remove" | "delete" | "uninstall" => Some("remove an installed skill from disk"),
        "repo" | "source" | "sources" => Some("add or change a skill source repository on disk"),
        // `/skills research` performs an egress web search and writes the egress
        // audit, a staging checkout, and review proposals — a persistent write,
        // not a read.
        "research" => Some("run a skill web search that writes an egress audit and proposals"),
        _ => None,
    }
}

impl SlashAction {
    /// Classify what this action would persist, so an incognito session can
    /// refuse every [`Persistence::Persistent`] one. Exhaustive by construction.
    #[must_use]
    pub fn persistence(&self) -> Persistence {
        match self {
            // Pure view/control: nothing written.
            Self::SetMode(_)
            | Self::SetProfile(_)
            | Self::ToggleThinking
            | Self::Clear
            | Self::Compact { .. }
            | Self::SetEffort(_)
            | Self::Tree
            | Self::Sessions
            | Self::HarnessResume
            | Self::WaitResume
            | Self::Knowledge(_)
            | Self::Agents(_)
            | Self::Background(_)
            | Self::LocalBoxModels
            | Self::Exit { .. }
            | Self::Help
            | Self::Theme(_)
            | Self::Settings(_)
            | Self::Diff(_)
            | Self::Search(_)
            | Self::LocalMind
            | Self::Incognito { .. }
            | Self::Model { .. }
            | Self::Invalid { .. }
            | Self::Unknown(_) => Persistence::ReadOnly,

            // Touch the session store only — in-memory under incognito, so these
            // leave nothing on disk there.
            Self::NewSession
            | Self::Fork
            | Self::CloneSession
            | Self::LoadSession(_)
            | Self::ContinueSession(_)
            | Self::NameSession(_) => Persistence::MemoryOnly,

            // Write to disk (or start a durable service) that outlives the run.
            Self::Research(_) => Persistence::Persistent("save a research report to disk"),
            Self::Ingest(_) => {
                Persistence::Persistent("ingest content into the persistent knowledge store")
            }
            Self::ContextBuild(_) => {
                Persistence::Persistent("build a persistent knowledge index on disk")
            }
            Self::LocalBoxAdopt { .. } => {
                Persistence::Persistent("write a LocalBox server into `.localpilot.toml`")
            }
            Self::LocalBoxServe { .. } => Persistence::Persistent(
                "start a durable LocalBox server and write it into `.localpilot.toml`",
            ),
            Self::SelfImprove(action) => match action {
                SelfImproveAction::Status => Persistence::ReadOnly,
                SelfImproveAction::Start { .. }
                | SelfImproveAction::Next
                | SelfImproveAction::Approve { .. }
                | SelfImproveAction::Reset => {
                    Persistence::Persistent("advance the persisted self-improvement loop on disk")
                }
            },
            Self::Skills(arg) => match skills_arg_writes(arg) {
                Some(what) => Persistence::Persistent(what),
                None => Persistence::ReadOnly,
            },
        }
    }
}

impl SlashAction {
    /// Whether this action is safe to execute while the selected host is
    /// driving an active model turn.
    #[must_use]
    pub const fn runs_live(&self, host: Host) -> bool {
        let shared = matches!(
            self,
            Self::SetProfile(_) | Self::ToggleThinking | Self::SetEffort(_) | Self::Background(_)
        );

        (shared && matches!(host, Host::Fullscreen))
            || matches!(
                (host, self),
                (
                    Host::Fullscreen,
                    Self::Exit { .. } | Self::Help | Self::Theme(_) | Self::Search(_)
                )
            )
    }
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
    /// budget. `parse_slash_for` reads this instead of re-matching the spelling
    /// string, so the `compact_force`/`compact-force` distinction lives in the
    /// table, not in the parser.
    pub force: bool,
    pub fullscreen: Option<&'static str>,
    pub pair: Option<&'static str>,
}

impl Spelling {
    /// Full-screen and pair, sharing one description.
    const fn fullscreen_and_pair(
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
            fullscreen: Some(desc),
            pair: Some(desc),
        }
    }

    /// A full-screen/pair takeover: its own per-host
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
            fullscreen: Some(fullscreen),
            pair: Some(pair),
        }
    }

    /// Full-screen only, no pair row.
    const fn fullscreen_only(
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
            fullscreen: Some(desc),
            pair: None,
        }
    }

    /// The permanent pair-only command. Its argument syntax describes the pair
    /// host that owns it; `parse_slash_for` never routes it outside pair.
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
            fullscreen: None,
            pair: None,
        }
    }

    const fn description_for(&self, host: Host) -> Option<&'static str> {
        match host {
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
        // Interactive full-screen command identities.
        Agent, Harness, Default, Relaxed, Bypass, Unrestricted, Think, Effort,
        Model, Localbox, Selfimprove, New, Fork, Clone, Tree, Sessions, Session, Name,
        Continue, Clear, Compact, HarnessResume, WaitResume, Ingest, Knowledge,
        Context, Research, Agents, Skills, Bg, Exit,
        // Full-screen/pair takeover identities: `parse_slash_for(Fullscreen|Pair)`
        // routes them to real actions; their catalog scope stays full-screen/pair-only.
        Help, Theme, Settings, Diff, Search,
        // Full-screen-only takeover; the pair host keeps it Unknown.
        LocalMind, Incognito,
        // Permanent pair-only identity: `/abort` is owned by the pair event
        // loop, never parsed by `parse_slash_for`, present only in the pair picker.
        Abort,
    }
    spellings {
        // --- the shared full-screen rows, in the frozen order ----------------
        Agent => fullscreen_only("agent", NoArg, Reject, "Switch to agent mode"),
        Harness => fullscreen_only("harness", NoArg, Reject, "Switch to harness mode"),
        // The four permission profiles and `/effort` are switchable in the
        // full-screen host (they update the runtime engine + projection).
        Default => fullscreen_only("default", NoArg, Reject, "Use the default permission profile"),
        Relaxed => fullscreen_only("relaxed", NoArg, Reject, "Use the relaxed permission profile"),
        Bypass => fullscreen_only("bypass", NoArg, Reject, "Use the bypass permission profile"),
        Unrestricted => fullscreen_only(
            "unrestricted",
            NoArg,
            Reject,
            "Approve everything, workspace boundary included — you take responsibility"
        ),
        // `/think` shows or hides reasoning items in the full-screen timeline.
        Think => fullscreen_only("think", NoArg, Reject, "Show or hide reasoning in the timeline"),
        Effort => fullscreen_only("effort", Required, Fall, "Set reasoning effort: minimal|low|medium|high"),
        Model => fullscreen_only(
            "model",
            Optional,
            Fall,
            "Switch provider/model, or list them (/model [provider [model]])"
        ),
        Localbox => fullscreen_only(
            "localbox",
            Optional,
            Fall,
            "List, serve, or adopt LocalBox (/localbox models|serve <model>|adopt)"
        ),
        Selfimprove => fullscreen_only(
            "selfimprove",
            Optional,
            Fall,
            "Review, propose, approve, build, and reload (/selfimprove [status|start|next|approve|reset])"
        ),
        New => fullscreen_only("new", NoArg, Fall, "Start a fresh session"),
        Fork => fullscreen_only("fork", NoArg, Fall, "Branch the conversation into a new session"),
        Clone => fullscreen_only("clone", NoArg, Fall, "Copy the conversation into a new session"),
        Tree => fullscreen_only("tree", NoArg, Fall, "Show the session event tree"),
        Sessions => fullscreen_only("sessions", NoArg, Fall, "List this workspace's sessions"),
        Session => fullscreen_only("session", Required, Fall, "Resume a session by id"),
        Name => fullscreen_only("name", Required, Fall, "Name this session (/name <text>)"),
        Name => fullscreen_only("rename", Required, Fall, "Rename this session (/rename <text>)"),
        Continue => fullscreen_only("continue", Optional, Fall, "Continue the previous session"),
        Clear => fullscreen_only("clear", NoArg, Reject, "Clear the conversation view"),
        Compact => fullscreen_only("compact", Optional, Fall, "Summarize and compact the context"),
        Compact => parse_forcing("compact_force", NoArg, Reject),
        Continue => fullscreen_only("resume", Optional, Fall, "Continue a previous session"),
        HarnessResume => fullscreen_only("harness-resume", NoArg, Reject, "Resume harness plan work"),
        WaitResume => fullscreen_only("wait-resume", NoArg, Reject, "Wait for quota, then resume"),
        Ingest => fullscreen_only("ingest", Optional, Fall, "Manage workspace ingestion"),
        Knowledge => fullscreen_only("knowledge", Required, Fall, "Query the knowledge base"),
        Context => fullscreen_only("context", Required, Fall, "Build a context bundle"),
        Research => fullscreen_only(
            "research",
            Optional,
            Fall,
            "Research a topic, local + web per config (/research [topic])"
        ),
        Agents => fullscreen_only(
            "agents",
            Optional,
            Fall,
            "List or inspect subagent definitions (/agents [show <name>])"
        ),
        // `/skills` opens the skills surface with no subcommand (`Skills("")`),
        // so it is Optional, not Required.
        Skills => fullscreen_only(
            "skills",
            Optional,
            Fall,
            "Manage skills: repos, install, list (/skills <subcommand>)"
        ),
        Bg => fullscreen_only("bg", Optional, Fall, "List background processes (/bg stop <id>|all)"),
        Exit => fullscreen_and_pair("exit", Optional, Reject, "Exit LocalPilot (/exit [print])"),
        // `/quit` accepts the same optional `print` argument as `/exit`.
        Exit => fullscreen_and_pair("quit", Optional, Reject, "Exit LocalPilot"),
        // --- full-screen/pair takeovers (per-host copy) ----------------------
        // These five route to real actions in both hosts. The
        // `ArgSpec`/`StrayArgs` metadata stays host-independent and
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
        LocalMind => fullscreen_only(
            "localmind",
            NoArg,
            Reject,
            "Browse LocalMind docs, graph, memory, review, skills, and audit"
        ),
        Incognito => fullscreen_only(
            "incognito",
            Optional,
            Fall,
            "Incognito: save nothing; new files need approval (`/incognito off` to end)"
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
            SlashAction::LocalBoxAdopt { .. } => C::Localbox,
            SlashAction::LocalBoxModels | SlashAction::LocalBoxServe { .. } => C::Localbox,
            SlashAction::SelfImprove(_) => C::Selfimprove,
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
            SlashAction::LocalMind => C::LocalMind,
            SlashAction::Incognito { .. } => C::Incognito,
            SlashAction::Invalid { .. } | SlashAction::Unknown(_) => return None,
        })
    }
}

/// Host-aware parse. Lookup-first: the name is resolved to its [`Spelling`] once,
/// then dispatched on the spelling's typed [`SlashCommand`] through one
/// wildcard-free match. An unknown name is `Unknown`. The five takeover spellings
/// resolve to their real actions ([`SlashAction::Help`]/`Theme`/`Settings`/`Diff`/
/// `Search`) in both hosts; the full-screen-only `/localmind` stays `Unknown` in
/// the pair host. The pair-only `abort` stays `Unknown` for every host — the pair
/// loop owns it as an exact-token route ahead of the parser.
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

fn parse_selfimprove(name: &str, args: &str) -> SlashAction {
    const USAGE: &str =
        "usage: /selfimprove [status | start [finding-rank] | next | approve <reviewer> | reset]";

    match args {
        "" | "status" => SlashAction::SelfImprove(SelfImproveAction::Status),
        "start" => SlashAction::SelfImprove(SelfImproveAction::Start { finding: None }),
        "next" => SlashAction::SelfImprove(SelfImproveAction::Next),
        "reset" => SlashAction::SelfImprove(SelfImproveAction::Reset),
        _ => {
            if let Some(rank) = args.strip_prefix("start ").map(str::trim) {
                return match rank.parse::<usize>() {
                    Ok(finding) if finding > 0 => {
                        SlashAction::SelfImprove(SelfImproveAction::Start {
                            finding: Some(finding),
                        })
                    }
                    _ => SlashAction::Invalid {
                        command: name.to_string(),
                        reason: "finding rank must be a positive integer".to_string(),
                    },
                };
            }
            if let Some(reviewer) = args.strip_prefix("approve ").map(str::trim) {
                return if reviewer.is_empty() {
                    SlashAction::Invalid {
                        command: name.to_string(),
                        reason: "approval requires the human reviewer's name".to_string(),
                    }
                } else {
                    SlashAction::SelfImprove(SelfImproveAction::Approve {
                        reviewer: reviewer.to_string(),
                    })
                };
            }
            SlashAction::Invalid {
                command: name.to_string(),
                reason: USAGE.to_string(),
            }
        }
    }
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
            fn serve_target(value: &str) -> Option<(String, bool)> {
                let value = value.trim();
                let (model, allow_untuned) = value
                    .strip_suffix(" --allow-untuned")
                    .map_or((value, false), |model| (model.trim(), true));
                (!model.is_empty()).then(|| (model.to_string(), allow_untuned))
            }

            if args.is_empty() || args == "adopt" {
                SlashAction::LocalBoxAdopt { serve: None }
            } else if args == "models" {
                SlashAction::LocalBoxModels
            } else if let Some(rest) = args
                .strip_prefix("serve")
                .filter(|rest| rest.chars().next().is_some_and(char::is_whitespace))
            {
                serve_target(rest).map_or_else(
                    || SlashAction::Invalid {
                        command: name.to_string(),
                        reason: "usage: /localbox models | serve <model> [--allow-untuned] | adopt"
                            .to_string(),
                    },
                    |(model, allow_untuned)| SlashAction::LocalBoxServe {
                        model,
                        allow_untuned,
                    },
                )
            } else if let Some(rest) = args
                .strip_prefix("adopt")
                .map(str::trim_start)
                .and_then(|rest| rest.strip_prefix("--serve"))
                .filter(|rest| rest.chars().next().is_some_and(char::is_whitespace))
            {
                serve_target(rest).map_or_else(
                    || SlashAction::Invalid {
                        command: name.to_string(),
                        reason: "usage: /localbox models | serve <model> [--allow-untuned] | adopt"
                            .to_string(),
                    },
                    |(model, allow_untuned)| SlashAction::LocalBoxServe {
                        model,
                        allow_untuned,
                    },
                )
            } else {
                SlashAction::Invalid {
                    command: name.to_string(),
                    reason: "usage: /localbox models | serve <model> [--allow-untuned] | adopt"
                        .to_string(),
                }
            }
        }
        C::Selfimprove => parse_selfimprove(name, args),
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
        // The five full-screen/pair takeovers resolve to their real actions in
        // both surviving hosts.
        // `help` is no-arg: the table-driven `no_arg`/`stray` path yields `Help`
        // for a bare form and the exact `Invalid` reason for a stray argument, so
        // the no-arg policy is not duplicated here.
        C::Help => no_arg(spelling, name, args, command, SlashAction::Help),
        C::Theme => SlashAction::Theme(opt_arg(args)),
        C::Settings => SlashAction::Settings(opt_arg(args)),
        C::Diff => SlashAction::Diff(opt_arg(args)),
        C::Search => SlashAction::Search(opt_arg(args)),
        C::LocalMind => routed_fullscreen(host, command, || {
            no_arg(spelling, name, args, command, SlashAction::LocalMind)
        }),
        C::Incognito => routed_fullscreen(host, command, || SlashAction::Incognito {
            off: args.trim().eq_ignore_ascii_case("off"),
        }),
    }
}

/// A trimmed argument tail as an `Option`: `None` when empty.
fn opt_arg(args: &str) -> Option<String> {
    (!args.is_empty()).then(|| args.to_string())
}

/// Resolve a full-screen-only takeover; the pair host truthfully sees it as
/// unavailable rather than inheriting the pair takeover scope.
fn routed_fullscreen(
    host: Host,
    command: &str,
    build: impl FnOnce() -> SlashAction,
) -> SlashAction {
    match host {
        Host::Fullscreen => build(),
        Host::Pair => SlashAction::Unknown(command.to_string()),
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

    /// Full-screen parse — the canonical single-host route these tests assert on.
    fn parse(line: &str) -> Option<SlashAction> {
        parse_slash_for(Host::Fullscreen, line)
    }

    #[test]
    fn persistent_actions_are_refused_by_incognito_and_name_what_they_write() {
        // A representative persistent member of each family is refused with a
        // non-empty explanation.
        let persistent = [
            SlashAction::Research(Some("topic".into())),
            SlashAction::Ingest(IngestAction::Run),
            SlashAction::ContextBuild("q".into()),
            SlashAction::LocalBoxAdopt { serve: None },
            SlashAction::LocalBoxServe {
                model: "m".into(),
                allow_untuned: false,
            },
            SlashAction::SelfImprove(SelfImproveAction::Next),
            SlashAction::Skills("install foo".into()),
        ];
        for action in persistent {
            let what = action.persistence().persistent_target();
            assert!(
                what.is_some_and(|w| !w.is_empty()),
                "{action:?} must be persistent with a message"
            );
        }
    }

    #[test]
    fn read_and_memory_actions_are_not_refused_by_incognito() {
        // Reads/views write nothing; session-store actions are in-memory under
        // incognito, so neither is a persistent action.
        for action in [
            SlashAction::Tree,
            SlashAction::Sessions,
            SlashAction::SelfImprove(SelfImproveAction::Status),
            SlashAction::Skills(String::new()),
            SlashAction::Skills("list".into()),
            SlashAction::LocalMind,
            SlashAction::SetProfile(Profile::Bypass),
        ] {
            assert_eq!(action.persistence().persistent_target(), None, "{action:?}");
        }
        for action in [
            SlashAction::NewSession,
            SlashAction::Fork,
            SlashAction::NameSession("x".into()),
        ] {
            assert_eq!(action.persistence(), Persistence::MemoryOnly, "{action:?}");
        }
    }

    #[test]
    fn skills_write_verbs_are_persistent_but_reads_are_not() {
        assert!(skills_arg_writes("install foo").is_some());
        assert!(skills_arg_writes("remove foo").is_some());
        assert!(skills_arg_writes("repo add url").is_some());
        assert!(skills_arg_writes("research query").is_some());
        assert!(skills_arg_writes("").is_none());
        assert!(skills_arg_writes("list").is_none());
    }

    #[test]
    fn live_actions_are_host_aware() {
        let shared = [
            SlashAction::SetProfile(Profile::Relaxed),
            SlashAction::ToggleThinking,
            SlashAction::SetEffort("high".to_string()),
            SlashAction::Background(BackgroundCommand::List),
        ];
        for action in shared {
            assert!(action.runs_live(Host::Fullscreen));
            assert!(!action.runs_live(Host::Pair));
        }

        for action in [
            SlashAction::Exit {
                print_transcript: false,
            },
            SlashAction::Help,
            SlashAction::Theme(None),
            SlashAction::Search(None),
        ] {
            assert!(action.runs_live(Host::Fullscreen));
            assert!(!action.runs_live(Host::Pair));
        }

        for action in [SlashAction::Clear, SlashAction::Settings(None)] {
            assert!(!action.runs_live(Host::Fullscreen));
            assert!(!action.runs_live(Host::Pair));
        }
    }

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
        assert_eq!(from_enum.len(), 39, "expected 39 command identities");
    }

    #[test]
    fn catalogs_have_the_frozen_cardinalities() {
        // Full-screen grew 19→24 (profiles + `/effort`), 24→25 (`/think`), 25→31
        // (the six synchronous commands `tree`/`knowledge`/`context`/`agents`/
        // `skills`/`bg`), 31→33 (`compact` + `ingest` on the operation pump),
        // 33→34 (`research` on the pump), 34→36 (`harness-resume` + `wait-resume`
        // on the pump), then 36→38 (`agent` + `harness` mode entries). `compact_force`
        // (a redundant forcing alias of `compact`) and the `wait_resume`/`compact-force`
        // parse-only aliases stay hidden but remain typeable in full-screen.
        // `/localmind` adds the one full-screen-only six-section workspace tab.
        assert_eq!(specs_for(Host::Fullscreen).len(), 41);
        assert_eq!(specs_for(Host::Pair).len(), 8);
    }

    #[test]
    fn full_screen_pumps_parse_regardless_of_picker_visibility() {
        // Host visibility controls picker projection, not lookup/dispatch.
        assert!(matches!(
            parse_slash_for(Host::Fullscreen, "/compact"),
            Some(SlashAction::Compact { force: false })
        ));
        assert!(matches!(
            parse_slash_for(Host::Fullscreen, "/compact force"),
            Some(SlashAction::Compact { force: true })
        ));
        assert!(matches!(
            parse_slash_for(Host::Fullscreen, "/compact_force"),
            Some(SlashAction::Compact { force: true })
        ));
        assert!(matches!(
            parse_slash_for(Host::Fullscreen, "/compact-force"),
            Some(SlashAction::Compact { force: true })
        ));
        let full_screen: BTreeSet<_> = specs_for(Host::Fullscreen)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert!(
            full_screen.contains("compact"),
            "base compact is a picker row"
        );
        assert!(
            !full_screen.contains("compact_force"),
            "compact_force stays hidden but is still typeable"
        );
        assert!(full_screen.contains("ingest"), "ingest is now a picker row");
        // The three long-running ingest forms parse in full-screen for the pump route.
        assert!(matches!(
            parse_slash_for(Host::Fullscreen, "/ingest"),
            Some(SlashAction::Ingest(IngestAction::Run))
        ));
        assert!(matches!(
            parse_slash_for(Host::Fullscreen, "/ingest refresh"),
            Some(SlashAction::Ingest(IngestAction::Refresh))
        ));
        assert!(matches!(
            parse_slash_for(Host::Fullscreen, "/ingest resume"),
            Some(SlashAction::Ingest(IngestAction::Resume))
        ));
    }

    #[test]
    fn ingest_tier_classifies_all_fourteen_variants() {
        use IngestAction::{
            Cancel, Exclude, Forget, Include, Pause, Preview, Promote, Rebuild, Refresh, Resume,
            Review, Run, Skipped, Status,
        };
        let long = [Run, Refresh, Resume];
        for action in &long {
            assert_eq!(action.tier(), IngestTier::LongRunning, "{action:?}");
        }
        let fast = [
            Preview,
            Status,
            Pause,
            Cancel,
            Rebuild,
            Skipped,
            Include("x".to_string()),
            Exclude("x".to_string()),
            Forget("x".to_string()),
            Review,
            Promote("x".to_string()),
        ];
        for action in &fast {
            assert_eq!(action.tier(), IngestTier::Fast, "{action:?}");
        }
        assert_eq!(long.len() + fast.len(), 14, "all 14 IngestAction variants");
    }

    #[test]
    fn full_screen_catalog_hides_the_redundant_forcing_alias() {
        let full_screen: BTreeSet<_> = specs_for(Host::Fullscreen)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        // A hidden forcing alias must not appear in the full-screen picker.
        // `compact_force` is a redundant forcing alias of `compact`, intentionally
        // hidden (typeable via `/compact force`), never a duplicate picker row.
        assert!(
            !full_screen.contains("compact_force"),
            "compact_force (a redundant forcing alias) leaked into the full-screen picker"
        );
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
        // Representative parse forms exercising the corrected metadata.
        assert_eq!(
            parse("/skills"),
            Some(SlashAction::Skills(String::new())),
            "/skills is Optional -> Skills(\"\")",
        );
        assert_eq!(
            parse("/quit print"),
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
        for host in [Host::Fullscreen, Host::Pair] {
            assert_eq!(
                parse_slash_for(host, "/abort"),
                Some(SlashAction::Unknown("abort".to_string())),
                "abort must stay external for {host:?}",
            );
        }
        let pair: BTreeSet<_> = specs_for(Host::Pair).into_iter().map(|(n, _)| n).collect();
        assert!(pair.contains("abort"));
        assert!(!specs_for(Host::Fullscreen)
            .iter()
            .any(|(n, _)| *n == "abort"));
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
    fn localmind_is_fullscreen_only_and_rejects_arguments() {
        assert_eq!(
            parse_slash_for(Host::Fullscreen, "/localmind"),
            Some(SlashAction::LocalMind)
        );
        assert_eq!(
            parse_slash_for(Host::Fullscreen, "/localmind extra"),
            Some(SlashAction::Invalid {
                command: "localmind".to_string(),
                reason: "this command does not take arguments".to_string(),
            })
        );
        assert_eq!(
            parse_slash_for(Host::Pair, "/localmind"),
            Some(SlashAction::Unknown("localmind".to_string()))
        );
        assert!(!specs_for(Host::Pair)
            .iter()
            .any(|(name, _)| *name == "localmind"));
        assert!(specs_for(Host::Fullscreen)
            .iter()
            .any(|(name, _)| *name == "localmind"));
        assert_eq!(SlashAction::LocalMind.command(), Some(C::LocalMind));
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
    fn hidden_but_parseable_aliases_map_to_their_action_and_stay_out_of_full_screen() {
        // Generated from spelling metadata, not a hard-coded list: a row is a
        // hidden-but-parseable full-screen alias iff it has NO full-screen catalog
        // description yet still parses to a real action on the full-screen host. That
        // set is exactly the parse-only spellings (`thinking`/`compact-force`/
        // `wait_resume`/`q`) plus the forcing alias `compact_force`. Pair-only `abort`
        // has no full-screen parse, so it is correctly EXCLUDED (never a full-screen
        // alias, never promoted).
        let full_screen: BTreeSet<_> = specs_for(Host::Fullscreen)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        // No command-identity special case: a row qualifies purely by (a) having NO
        // full-screen catalog description and (b) PARSING to a real semantic action on
        // the full-screen host. Pair-only `abort` is excluded NATURALLY by (b) — its
        // full-screen parse yields no semantic command — never by naming its id.
        let mut aliases: Vec<&str> = Vec::new();
        for spelling in SLASH_SPELLINGS {
            if spelling.description_for(Host::Fullscreen).is_some() {
                continue; // visible in the full-screen catalog — not a hidden alias.
            }
            let line = match spelling.args {
                ArgSpec::Required => format!("/{} x", spelling.name),
                ArgSpec::None | ArgSpec::Optional => format!("/{}", spelling.name),
            };
            // Retain only rows that parse to a real semantic action (a `command()`,
            // never `Unknown`). This is what excludes `abort` — no naming it.
            let action = match parse_slash_for(Host::Fullscreen, &line) {
                Some(action) if action.command().is_some() => action,
                _ => continue,
            };
            assert_eq!(
                action.command(),
                Some(spelling.command),
                "hidden alias {} parses to the wrong action: {action:?}",
                spelling.name
            );
            assert!(
                !full_screen.contains(spelling.name),
                "hidden alias {} leaked into the full-screen visible catalog",
                spelling.name
            );
            aliases.push(spelling.name);
        }
        // Lock the exact set so a new hidden alias — or an accidental promotion — is caught.
        aliases.sort_unstable();
        assert_eq!(
            aliases,
            [
                "compact-force",
                "compact_force",
                "q",
                "thinking",
                "wait_resume"
            ],
            "hidden-but-parseable full-screen aliases drifted"
        );
        // Explicit `/abort` guard: pair-owned, so its full-screen parse has NO semantic
        // action, and it never appears in the full-screen visible catalog.
        let abort = parse_slash_for(Host::Fullscreen, "/abort");
        assert!(
            matches!(abort, Some(SlashAction::Unknown(_))),
            "/abort must parse to `Unknown` on the full-screen host (pair-owned, no shared route), got {abort:?}"
        );
        assert!(
            !full_screen.contains("abort"),
            "pair-only abort leaked into the full-screen picker"
        );
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
                    parse(&format!("/{name} stray")),
                    Some(SlashAction::Invalid { .. })
                ),
                "/{name} stray should be Invalid",
            );
        }
        // The five no-arg commands omitted from the guard fall through to Unknown.
        for name in ["new", "fork", "clone", "tree", "sessions"] {
            assert!(
                matches!(
                    parse(&format!("/{name} stray")),
                    Some(SlashAction::Unknown(_))
                ),
                "/{name} stray should be Unknown",
            );
        }
    }

    #[test]
    fn alias_sensitive_forms_parse_exactly() {
        assert_eq!(
            parse("/compact"),
            Some(SlashAction::Compact { force: false })
        );
        assert_eq!(
            parse("/compact force"),
            Some(SlashAction::Compact { force: true })
        );
        assert_eq!(
            parse("/compact_force"),
            Some(SlashAction::Compact { force: true })
        );
        assert_eq!(
            parse("/compact-force"),
            Some(SlashAction::Compact { force: true })
        );
        assert!(matches!(
            parse("/compact bogus"),
            Some(SlashAction::Invalid { .. })
        ));
        assert_eq!(
            parse("/exit"),
            Some(SlashAction::Exit {
                print_transcript: false
            })
        );
        assert_eq!(
            parse("/exit print"),
            Some(SlashAction::Exit {
                print_transcript: true
            })
        );
        assert_eq!(
            parse("/quit"),
            Some(SlashAction::Exit {
                print_transcript: false
            })
        );
        assert_eq!(
            parse("/q print"),
            Some(SlashAction::Exit {
                print_transcript: true
            })
        );
        assert_eq!(
            parse("/name a"),
            Some(SlashAction::NameSession("a".to_string())),
        );
        assert_eq!(
            parse("/rename a"),
            Some(SlashAction::NameSession("a".to_string())),
        );
        assert_eq!(parse("/continue"), Some(SlashAction::ContinueSession(None)),);
        assert_eq!(
            parse("/resume x"),
            Some(SlashAction::ContinueSession(Some("x".to_string()))),
        );
        assert_eq!(parse("/think"), Some(SlashAction::ToggleThinking));
        // The hidden `/thinking` alias shares the action (no picker row) on every
        // host — `/think` is a shared command, not a host-gated takeover.
        assert_eq!(parse("/thinking"), Some(SlashAction::ToggleThinking));
        assert_eq!(
            parse_slash_for(Host::Fullscreen, "/thinking"),
            Some(SlashAction::ToggleThinking)
        );
        assert_eq!(
            parse_slash_for(Host::Fullscreen, "/think"),
            Some(SlashAction::ToggleThinking)
        );
        assert_eq!(parse("/wait-resume"), Some(SlashAction::WaitResume));
        assert_eq!(parse("/wait_resume"), Some(SlashAction::WaitResume));
    }

    #[test]
    fn localbox_teaches_models_and_direct_serve_while_retaining_adopt_compatibility() {
        assert_eq!(
            parse("/localbox"),
            Some(SlashAction::LocalBoxAdopt { serve: None })
        );
        assert_eq!(
            parse("/localbox adopt"),
            Some(SlashAction::LocalBoxAdopt { serve: None })
        );
        assert_eq!(
            parse("/localbox adopt --serve Bonsai 27B.gguf"),
            Some(SlashAction::LocalBoxServe {
                model: "Bonsai 27B.gguf".to_string(),
                allow_untuned: false,
            })
        );
        assert_eq!(parse("/localbox models"), Some(SlashAction::LocalBoxModels));
        assert_eq!(
            parse("/localbox serve apex"),
            Some(SlashAction::LocalBoxServe {
                model: "apex".to_string(),
                allow_untuned: false,
            })
        );
        assert_eq!(
            parse("/localbox serve apex --allow-untuned"),
            Some(SlashAction::LocalBoxServe {
                model: "apex".to_string(),
                allow_untuned: true,
            })
        );
        for malformed in [
            "/localbox --serve model",
            "/localbox adopt --serve",
            "/localbox adopt --server model",
        ] {
            assert!(matches!(
                parse(malformed),
                Some(SlashAction::Invalid { .. })
            ));
        }
    }

    #[test]
    fn selfimprove_preserves_explicit_rank_reviewer_and_gate_actions() {
        assert_eq!(
            parse("/selfimprove"),
            Some(SlashAction::SelfImprove(SelfImproveAction::Status))
        );
        assert_eq!(
            parse("/selfimprove status"),
            Some(SlashAction::SelfImprove(SelfImproveAction::Status))
        );
        assert_eq!(
            parse("/selfimprove start"),
            Some(SlashAction::SelfImprove(SelfImproveAction::Start {
                finding: None
            }))
        );
        assert_eq!(
            parse("/selfimprove start 12"),
            Some(SlashAction::SelfImprove(SelfImproveAction::Start {
                finding: Some(12)
            }))
        );
        assert_eq!(
            parse("/selfimprove next"),
            Some(SlashAction::SelfImprove(SelfImproveAction::Next))
        );
        assert_eq!(
            parse("/selfimprove approve David Smith"),
            Some(SlashAction::SelfImprove(SelfImproveAction::Approve {
                reviewer: "David Smith".to_string()
            }))
        );
        assert_eq!(
            parse("/selfimprove reset"),
            Some(SlashAction::SelfImprove(SelfImproveAction::Reset))
        );
        assert!(matches!(
            parse("/selfimprove start 0"),
            Some(SlashAction::Invalid { .. })
        ));
        assert!(matches!(
            parse("/selfimprove approve"),
            Some(SlashAction::Invalid { .. })
        ));
    }
}
