//! `localpilot search setup`: set up a search backend LocalPilot manages.
//!
//! The person at this terminal picks a backend from a list that says what each
//! needs and where queries go, sees the exact program, arguments and config
//! that will be written, and confirms a second time. Setup then starts the
//! backend once (this downloads the pinned package and checks its search tool),
//! and only after that writes one `[search.managed.<id>]` table to the user
//! config in a single locked edit, and reads the effective config back.
//!
//! A key for a hosted search API goes into a fresh credential alias created by
//! this run, never into the config, and never over a credential something else
//! already uses. Each step is recorded in a per-user audit log before it runs.

use std::path::{Path, PathBuf};

use localpilot_config::{
    Config, CredentialStore, ManagedSearchBackend, McpEnvEntry, McpEnvObject, SearchTier,
};
use localpilot_core::Secret;

use crate::search_backend::{Launcher, SearchSpec};
use crate::user_config::{table_at, ConfigEdit, ConfigError};

/// The DuckDuckGo search package and the extra it needs to get past
/// DuckDuckGo's block on plain HTTP clients. Pinned; bumped deliberately.
const DDG_PACKAGE: &str = "duckduckgo-mcp-server==0.7.0";
const DDG_WITH_BROWSER: &str = "duckduckgo-mcp-server[browser]==0.7.0";
/// The official Brave and Tavily search packages. Pinned; bumped deliberately.
const BRAVE_PACKAGE: &str = "@brave/brave-search-mcp-server@2.1.5";
const TAVILY_PACKAGE: &str = "tavily-mcp@0.2.22";
/// The oldest Node.js the pinned npm packages run on.
const NODE_MIN_MAJOR: u32 = 22;

/// A backend setup can configure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Backend {
    DuckDuckGo,
    Brave,
    Tavily,
}

/// What a backend needs on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Needs {
    Uv,
    Node,
}

impl Backend {
    const ALL: [Self; 3] = [Self::DuckDuckGo, Self::Brave, Self::Tavily];

    /// The `[search.managed]` id.
    const fn id(self) -> &'static str {
        match self {
            Self::DuckDuckGo => "duckduckgo",
            Self::Brave => "brave",
            Self::Tavily => "tavily",
        }
    }

    const fn title(self) -> &'static str {
        match self {
            Self::DuckDuckGo => "DuckDuckGo, no key needed",
            Self::Brave => "Brave Search API, with your own key",
            Self::Tavily => "Tavily, with your own key",
        }
    }

    const fn tier(self) -> SearchTier {
        match self {
            Self::DuckDuckGo => SearchTier::Keyless,
            Self::Brave | Self::Tavily => SearchTier::ApiKey,
        }
    }

    const fn needs(self) -> Needs {
        match self {
            Self::DuckDuckGo => Needs::Uv,
            Self::Brave | Self::Tavily => Needs::Node,
        }
    }

    /// Where queries go and what that means, shown before anything is chosen.
    const fn privacy(self) -> &'static str {
        match self {
            Self::DuckDuckGo => {
                "Queries go to DuckDuckGo from your own IP address. The package reads \
                 DuckDuckGo's HTML results page, which is not an official API, so \
                 DuckDuckGo may slow it down or block it."
            }
            Self::Brave => {
                "Queries go to Brave under your account, subject to Brave's terms and \
                 your plan's quota."
            }
            Self::Tavily => {
                "Queries go to Tavily under your account, subject to Tavily's terms and \
                 your plan's quota."
            }
        }
    }

    /// The environment variable the key goes into, for a backend that needs one.
    const fn key_variable(self) -> Option<&'static str> {
        match self {
            Self::DuckDuckGo => None,
            Self::Brave => Some("BRAVE_API_KEY"),
            Self::Tavily => Some("TAVILY_API_KEY"),
        }
    }

    /// The managed entry, with \`alias\` as the key's credential where one is
    /// needed, and its launch identity recorded.
    fn entry(self, program: &Path, alias: Option<&str>) -> ManagedSearchBackend {
        let mut env = indexmap::IndexMap::new();
        let (args, tool, call_timeout_secs, max_query_words) = match self {
            Self::DuckDuckGo => {
                // Long result links would otherwise come back as reference
                // tokens, which are not links.
                env.insert(
                    "DDG_REF_URL_THRESHOLD".to_string(),
                    McpEnvEntry::Plain("0".to_string()),
                );
                (
                    vec![
                        "--with".to_string(),
                        DDG_WITH_BROWSER.to_string(),
                        DDG_PACKAGE.to_string(),
                    ],
                    "search",
                    // It waits out a rate limit for up to 30 seconds itself.
                    45,
                    None,
                )
            }
            Self::Brave => (
                vec![
                    "-y".to_string(),
                    BRAVE_PACKAGE.to_string(),
                    // Its default; stated so an environment variable cannot
                    // turn it into a network listener.
                    "--transport".to_string(),
                    "stdio".to_string(),
                ],
                "brave_web_search",
                20,
                Some(50),
            ),
            Self::Tavily => (
                vec!["-y".to_string(), TAVILY_PACKAGE.to_string()],
                "tavily_search",
                20,
                None,
            ),
        };
        if let (Some(variable), Some(alias)) = (self.key_variable(), alias) {
            env.insert(
                variable.to_string(),
                McpEnvEntry::Object(McpEnvObject {
                    value: None,
                    credential: Some(alias.to_string()),
                }),
            );
        }
        let mut entry = ManagedSearchBackend {
            tier: self.tier(),
            command: program.display().to_string(),
            args,
            env,
            tool: tool.to_string(),
            launch_identity: String::new(),
            call_timeout_secs,
            handshake_timeout_secs: 120,
            max_query_chars: 400,
            max_query_words,
            unattended: false,
        };
        entry.launch_identity = localpilot_config::search::launch_identity(&entry.server());
        entry
    }
}

/// What is installed on this machine.
pub(crate) trait Probes {
    /// The absolute path of `program` on PATH, from absolute PATH entries only:
    /// never the working directory or a relative entry, so a workspace cannot
    /// choose what runs.
    fn find(&self, program: &str) -> Option<PathBuf>;
    /// What `program --version` prints, if it runs and succeeds.
    fn version(&self, program: &Path) -> Option<String>;
}

/// The programs a backend runs, found on PATH and answering.
struct Found {
    uvx: Option<PathBuf>,
    node_major: Option<u32>,
    npx: Option<PathBuf>,
    container: Option<&'static str>,
}

impl Found {
    fn probe(probes: &dyn Probes) -> Self {
        let answering = |name: &str| {
            probes
                .find(name)
                .filter(|path| probes.version(path).is_some())
        };
        let node_major = probes.find("node").and_then(|node| {
            probes
                .version(&node)?
                .trim()
                .trim_start_matches('v')
                .split('.')
                .next()?
                .parse()
                .ok()
        });
        Self {
            uvx: answering("uvx"),
            node_major,
            npx: probes.find("npx"),
            container: ["docker", "podman"]
                .into_iter()
                .find(|name| answering(name).is_some()),
        }
    }

    fn node_ok(&self) -> bool {
        self.node_major.is_some_and(|major| major >= NODE_MIN_MAJOR) && self.npx.is_some()
    }

    /// The program `backend` is started with, when it is available.
    fn launcher_for(&self, backend: Backend) -> Option<&Path> {
        match backend.needs() {
            Needs::Uv => self.uvx.as_deref(),
            Needs::Node if self.node_ok() => self.npx.as_deref(),
            Needs::Node => None,
        }
    }
}

/// A numbered choice: one of the options, an explicit no, or no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pick {
    Option(usize),
    No,
    Cancel,
}

/// A yes/no answer; anything but a clear yes or no is `Cancel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Answer {
    Yes,
    No,
    Cancel,
}

/// What the person running setup sees and answers.
pub(crate) trait SetupConsole {
    fn say(&self, line: &str);
    fn pick(&self, prompt: &str, options: &[String]) -> Pick;
    fn ask(&self, prompt: &str) -> Answer;
    /// Read a secret without showing it; `None` when the person cancels or it
    /// cannot be read without showing it.
    fn secret(&self, prompt: &str) -> Option<Secret>;
}

/// How the user config is edited (the shared writer in production).
pub(crate) type ConfigWriter<'a> = &'a dyn Fn(
    Option<&Path>,
    &dyn Fn(&mut toml_edit::DocumentMut) -> Result<(), String>,
) -> Result<ConfigEdit, ConfigError>;

/// Everything setup touches, injected so tests use fakes.
pub(crate) struct Setup<'a> {
    pub(crate) console: &'a dyn SetupConsole,
    pub(crate) probes: &'a dyn Probes,
    pub(crate) credentials: &'a CredentialStore,
    pub(crate) config_path: Option<PathBuf>,
    /// `~/.localpilot/search`: the audit log and setup state.
    pub(crate) state_dir: PathBuf,
    pub(crate) launcher: Launcher,
    /// The effective config, read fresh.
    pub(crate) effective: &'a dyn Fn() -> anyhow::Result<Config>,
    /// Sixteen random hex digits, for a fresh credential alias.
    pub(crate) random_suffix: &'a dyn Fn() -> Option<String>,
    pub(crate) write: ConfigWriter<'a>,
    /// Whether a key may be asked for here. Only a terminal reads one
    /// without showing it; elsewhere a backend that needs a key is pointed at
    /// `localpilot search setup`.
    pub(crate) keys: bool,
    /// Whoever started setup has gone (the session closed): nothing more is
    /// stored, started or written.
    pub(crate) stopped: &'a dyn Fn() -> bool,
}

/// How a setup run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The backend is configured and the effective config shows it.
    Configured(String),
    /// The person said no to every backend.
    Declined,
    /// The person stopped before anything was set up.
    Cancelled,
    /// Setup stopped; the reason was shown.
    Stopped(String),
}

/// The audit log of setup runs.
fn audit_path(state_dir: &Path) -> PathBuf {
    state_dir.join("setup-audit.log")
}

/// Append one record; no record, no step.
fn audit(state_dir: &Path, event: &str, backend: Backend, detail: &str) -> bool {
    use std::io::Write as _;
    let path = audit_path(state_dir);
    if std::fs::create_dir_all(state_dir).is_err() {
        return false;
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return false;
    };
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    writeln!(
        file,
        "at={seconds} event={event} id={} tier={} {detail}",
        backend.id(),
        backend.tier().as_str()
    )
    .is_ok()
}

/// The marker an in-session offer leaves when the person said no; setup run
/// by hand clears it.
fn decline_marker(state_dir: &Path) -> PathBuf {
    state_dir.join("setup-declined")
}

/// Remember a "no, and don't offer again" to the in-session offer.
#[cfg(feature = "tui")]
pub(crate) fn remember_decline(state_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(state_dir)?;
    std::fs::write(decline_marker(state_dir), "declined\n")
}

/// Whether the in-session offer was declined for good.
#[cfg(feature = "tui")]
pub(crate) fn declined(state_dir: &Path) -> bool {
    decline_marker(state_dir).exists()
}

/// Forget an earlier "no" to the in-session offer.
pub(crate) fn clear_decline(state_dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(decline_marker(state_dir)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// The program line as it will run.
fn command_line(entry: &ManagedSearchBackend) -> String {
    std::iter::once(entry.command.as_str())
        .chain(entry.args.iter().map(String::as_str))
        .map(|part| {
            if part.contains([' ', '[', ']']) {
                format!("\"{part}\"")
            } else {
                part.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The `[search.managed.<id>]` table for \`entry\`.
fn entry_table(entry: &ManagedSearchBackend) -> toml_edit::Table {
    use toml_edit::{value, Array, InlineTable, Table};
    let mut table = Table::new();
    table["tier"] = value(entry.tier.as_str());
    table["command"] = value(entry.command.as_str());
    table["args"] = value(entry.args.iter().map(String::as_str).collect::<Array>());
    table["tool"] = value(entry.tool.as_str());
    table["launch_identity"] = value(entry.launch_identity.as_str());
    table["call_timeout_secs"] = value(i64::try_from(entry.call_timeout_secs).unwrap_or(i64::MAX));
    table["handshake_timeout_secs"] =
        value(i64::try_from(entry.handshake_timeout_secs).unwrap_or(i64::MAX));
    table["max_query_chars"] = value(i64::try_from(entry.max_query_chars).unwrap_or(i64::MAX));
    if let Some(words) = entry.max_query_words {
        table["max_query_words"] = value(i64::try_from(words).unwrap_or(i64::MAX));
    }
    table["unattended"] = value(entry.unattended);
    if !entry.env.is_empty() {
        let mut env = Table::new();
        for (name, item) in &entry.env {
            match item {
                McpEnvEntry::Plain(text) => env[name.as_str()] = value(text.as_str()),
                McpEnvEntry::Object(object) => {
                    let mut inline = InlineTable::new();
                    if let Some(alias) = &object.credential {
                        inline.insert("credential", alias.as_str().into());
                    }
                    env[name.as_str()] = value(inline);
                }
            }
        }
        table["env"] = toml_edit::Item::Table(env);
    }
    table
}

/// Put \`entry\` at `[search.managed.<id>]`, replacing what was there.
fn place(
    document: &mut toml_edit::DocumentMut,
    id: &str,
    entry: &ManagedSearchBackend,
) -> Result<(), String> {
    table_at(document, &["search", "managed"])?
        .insert(id, toml_edit::Item::Table(entry_table(entry)));
    Ok(())
}

/// The credential alias a managed entry reads its key from, if any.
fn alias_of(entry: &ManagedSearchBackend) -> Option<&str> {
    entry.env.values().find_map(|item| match item {
        McpEnvEntry::Object(object) => object.credential.as_deref(),
        McpEnvEntry::Plain(_) => None,
    })
}

/// Run setup with the person at this terminal.
pub(crate) async fn run(s: &Setup<'_>) -> Outcome {
    let say = |line: &str| s.console.say(line);
    // 1. What there is to choose from, and what each one means.
    let found = Found::probe(s.probes);
    let node_ok = found.node_ok();
    let uv_ok = found.uvx.is_some();
    say("Search backends LocalPilot can set up for web search and /research:");
    say("");
    say("Tier 1, no key:");
    say(&format!(
        "  {} ({})",
        Backend::DuckDuckGo.title(),
        if uv_ok { "uv found" } else { "needs uv" }
    ));
    say(&format!("    {}", Backend::DuckDuckGo.privacy()));
    say("Tier 2, a search engine on this machine:");
    say(&format!(
        "  SearXNG in a container ({}): not available in this version.",
        found.container.map_or_else(
            || "no Docker or Podman found".to_string(),
            |found| format!("{found} found")
        )
    ));
    say("Tier 3, your own key:");
    for backend in [Backend::Brave, Backend::Tavily] {
        say(&format!(
            "  {} ({})",
            backend.title(),
            if node_ok {
                "Node.js found".to_string()
            } else {
                format!("needs Node.js {NODE_MIN_MAJOR} or later")
            }
        ));
        say(&format!("    {}", backend.privacy()));
    }
    say("");
    let options: Vec<String> = Backend::ALL
        .iter()
        .map(|backend| {
            if !s.keys && backend.key_variable().is_some() {
                format!("{} (set up in a terminal)", backend.title())
            } else {
                backend.title().to_string()
            }
        })
        .collect();
    let backend = match s
        .console
        .pick("Which backend should LocalPilot set up?", &options)
    {
        Pick::Option(index) => match Backend::ALL.get(index) {
            Some(backend) => *backend,
            None => return Outcome::Cancelled,
        },
        Pick::No => {
            say("Nothing was set up.");
            return Outcome::Declined;
        }
        Pick::Cancel => {
            say("Nothing was set up.");
            return Outcome::Cancelled;
        }
    };

    if !s.keys && backend.key_variable().is_some() {
        let reason = format!(
            "{} needs your own key, and a key is entered only at a terminal: run localpilot search setup there",
            backend.title()
        );
        say(&reason);
        return Outcome::Stopped(reason);
    }

    // 2. What it needs.
    let program = match (backend.needs(), found.launcher_for(backend)) {
        (_, Some(program)) => program.to_path_buf(),
        (Needs::Uv, None) => {
            let reason = "uv is not installed. Install it (https://docs.astral.sh/uv/getting-started/installation/), then run localpilot search setup again.";
            say(reason);
            return Outcome::Stopped(reason.to_string());
        }
        (Needs::Node, None) => {
            let reason = format!(
                "Node.js {NODE_MIN_MAJOR} or later is needed (https://nodejs.org/en/download). Install it, then run localpilot search setup again."
            );
            say(&reason);
            return Outcome::Stopped(reason);
        }
    };

    // 3. A fresh alias for a key, never one something else may use.
    let alias = if backend.key_variable().is_some() {
        let Some(suffix) = (s.random_suffix)() else {
            return stop(s, "no random bytes were available for the credential name");
        };
        let alias = format!("search-{}-{suffix}", backend.id());
        if s.credentials.generic_get(&alias).is_some() {
            return stop(
                s,
                &format!("the credential {alias} already exists; nothing was changed"),
            );
        }
        Some(alias)
    } else {
        None
    };
    let mut entry = backend.entry(&program, alias.as_deref());
    let previous = (s.effective)()
        .ok()
        .and_then(|config| config.search.managed.get(backend.id()).cloned());

    // 4. Exactly what will happen.
    say("");
    say("Setup will:");
    say(&format!("  run: {}", command_line(&entry)));
    for (name, item) in &entry.env {
        match item {
            McpEnvEntry::Plain(text) => say(&format!("    with {name}={text}")),
            McpEnvEntry::Object(_) => say(&format!(
                "    with {name} from the stored credential {}",
                alias.as_deref().unwrap_or_default()
            )),
        }
    }
    say("  This runs third-party code on this computer with your rights. It is started once now to download it and check its search tool.");
    if let Some(alias) = &alias {
        say(&format!(
            "  ask for your key and keep it in the credential store as {alias} (never in the config file)"
        ));
    }
    let path_text = s.config_path.as_deref().map_or_else(
        || "(no user config location)".to_string(),
        |path| path.display().to_string(),
    );
    say(&format!("  write to {path_text}:"));
    let mut preview = toml_edit::DocumentMut::new();
    if place(&mut preview, backend.id(), &entry).is_ok() {
        for line in preview.to_string().lines() {
            say(&format!("    {line}"));
        }
    }
    if previous.is_some() {
        say(&format!(
            "  This replaces the {} backend set up before.",
            backend.id()
        ));
    }
    say("");
    if s.console.ask("Set this up now?") != Answer::Yes {
        say("Nothing was set up.");
        return Outcome::Cancelled;
    }

    // 5. Recorded before anything is stored or run.
    if !audit(
        &s.state_dir,
        "setup-start",
        backend,
        &format!("identity={}", entry.launch_identity),
    ) {
        return stop(
            s,
            "the setup audit record could not be written; nothing was done",
        );
    }

    // 6. The key, into its own new alias.
    if let (Some(alias), Some(variable)) = (&alias, backend.key_variable()) {
        let Some(secret) = s.console.secret(&format!(
            "Paste your key for {variable}; nothing is shown as you type (an empty line cancels): "
        )) else {
            audit(&s.state_dir, "setup-cancelled", backend, "at=key");
            say("Nothing was set up.");
            return Outcome::Cancelled;
        };
        if secret.expose().is_empty() {
            audit(&s.state_dir, "setup-cancelled", backend, "at=key");
            say("No key was entered. Nothing was set up.");
            return Outcome::Cancelled;
        }
        if let Some(outcome) = halted(s, backend, None, "key") {
            return outcome;
        }
        // The preview promised a fresh name; something may have taken it
        // while the questions were open.
        if s.credentials.generic_get(alias).is_some() {
            audit(
                &s.state_dir,
                "setup-failed",
                backend,
                "at=key reason=alias-taken",
            );
            return stop(
                s,
                &format!(
                    "the credential {alias} appeared while setup was waiting; nothing was changed"
                ),
            );
        }
        if let Err(error) = s.credentials.generic_set(alias, &secret) {
            audit(&s.state_dir, "setup-failed", backend, "at=key");
            return stop(s, &format!("the key could not be stored ({error})"));
        }
    }

    // 7. Start it once: download, handshake, and its search tool's shape.
    if let Some(outcome) = halted(s, backend, alias.as_deref(), "check") {
        return outcome;
    }
    say("Starting it once to download it and check its search tool (the first time can take a minute)...");
    let spec = SearchSpec {
        server: backend.id().to_string(),
        tool: entry.tool.clone(),
        launch: entry.server(),
        managed: Some(entry.clone()),
    };
    let started =
        crate::search_backend::start_managed(&spec, &entry, &s.launcher, |_, _| true).await;
    match started {
        Ok(transport) => drop(transport),
        Err(error) => {
            audit(
                &s.state_dir,
                "setup-failed",
                backend,
                &format!("at=check reason={}", error.decision()),
            );
            let left = forget_alias(s, backend, alias.as_deref());
            return stop(
                s,
                &format!(
                    "{}; the config was not changed{left}",
                    error.describe(backend.id())
                ),
            );
        }
    }
    say("It started and offers a search tool LocalPilot can use.");

    // 8. Unattended research, off unless asked for.
    if let Some(outcome) = halted(s, backend, alias.as_deref(), "unattended") {
        return outcome;
    }
    entry.unattended = match s
        .console
        .ask("Allow scheduled and unattended research to use this backend?")
    {
        Answer::Yes => true,
        Answer::No => false,
        Answer::Cancel => {
            audit(&s.state_dir, "setup-cancelled", backend, "at=unattended");
            let left = forget_alias(s, backend, alias.as_deref());
            say(&format!("Nothing was set up{left}."));
            return Outcome::Cancelled;
        }
    };

    // 9. One locked edit, then read back what is in effect.
    if let Some(outcome) = halted(s, backend, alias.as_deref(), "write") {
        return outcome;
    }
    let id = backend.id();
    match (s.write)(s.config_path.as_deref(), &|document| {
        if (s.stopped)() {
            return Err(STOPPED.to_string());
        }
        place(document, id, &entry)
    }) {
        Ok(ConfigEdit::Written | ConfigEdit::Unchanged) => {}
        Err(ConfigError::Before(_)) if (s.stopped)() => {
            return halted(s, backend, alias.as_deref(), "write").unwrap_or(Outcome::Cancelled);
        }
        Err(ConfigError::Before(reason)) => {
            audit(&s.state_dir, "setup-failed", backend, "at=write");
            let left = forget_alias(s, backend, alias.as_deref());
            return stop(s, &format!("the config was not changed ({reason}){left}"));
        }
        Err(ConfigError::After(reason)) => {
            // The table may be in effect: never delete a key it may need.
            audit(&s.state_dir, "setup-unconfirmed", backend, "at=write");
            let in_effect = (s.effective)().ok().is_some_and(|config| {
                config.search.managed.get(id).and_then(alias_of) == alias.as_deref()
                    && alias.is_some()
            });
            let mut message =
                format!("the config was written but could not be confirmed ({reason})");
            if let Some(alias) = &alias {
                if in_effect {
                    message.push_str(&format!("; it uses the credential {alias}, which was kept"));
                } else {
                    message.push_str(&format!(
                        "; the credential {alias} was kept: remove it with localpilot credential delete {alias} if the config does not use it"
                    ));
                }
            }
            return stop(s, &message);
        }
    }
    let config = match (s.effective)() {
        Ok(config) => config,
        Err(error) => {
            audit(&s.state_dir, "setup-unconfirmed", backend, "at=verify");
            return stop(
                s,
                &format!("the config was written but could not be read back ({error})"),
            );
        }
    };
    match config.search.managed.get(id) {
        Some(found) if *found == entry && found.identity_matches() => {}
        _ => {
            audit(&s.state_dir, "setup-unconfirmed", backend, "at=verify");
            return stop(s, "the config was written but the effective config does not show the backend as written");
        }
    }
    let (specs, _) = crate::search_backend::search_specs(&config);
    let first: Vec<String> = specs
        .iter()
        .take_while(|other| !(other.managed.is_some() && other.server == id))
        .map(SearchSpec::label)
        .collect();
    if !first.is_empty() {
        say(&format!(
            "Note: these search tools are tried before it: {} ([research.mcp] in your config or this project's).",
            first.join(", ")
        ));
    }
    for layer in &config.search_ignored {
        say(&format!("Note: [search] settings in the {layer} config are ignored; only your user config sets search backends."));
    }
    if let (Some(old), Some(new)) = (previous.as_ref().and_then(alias_of), alias.as_deref()) {
        if old != new {
            say(&format!(
                "The credential {old} is no longer used by this backend. Remove it with localpilot credential delete {old} if nothing else uses it."
            ));
        }
    }
    audit(
        &s.state_dir,
        "setup-done",
        backend,
        &format!(
            "identity={} unattended={}",
            entry.launch_identity, entry.unattended
        ),
    );
    say(&format!(
        "Done: web_search and /research now use {} ({}).",
        backend.title(),
        id
    ));
    Outcome::Configured(id.to_string())
}

/// Why a write was refused when setup was stopped part-way.
pub(crate) const STOPPED: &str = "setup was stopped";

/// When setup was stopped (its session closed), record where, remove the key
/// this run stored, and end. `None` while it may go on.
fn halted(s: &Setup<'_>, backend: Backend, alias: Option<&str>, at: &str) -> Option<Outcome> {
    if !(s.stopped)() {
        return None;
    }
    audit(
        &s.state_dir,
        "setup-cancelled",
        backend,
        &format!("at={at} reason=stopped"),
    );
    let left = forget_alias(s, backend, alias);
    s.console.say(&format!("Nothing was set up{left}."));
    Some(Outcome::Cancelled)
}

fn stop(s: &Setup<'_>, reason: &str) -> Outcome {
    s.console.say(&format!("Setup stopped: {reason}."));
    Outcome::Stopped(reason.to_string())
}

/// Remove a credential this run created and nothing was written to use.
/// Returns what is left behind, for the closing message: nothing, or the
/// credential and how to remove it when it could not be removed (recorded in
/// the audit log too).
fn forget_alias(s: &Setup<'_>, backend: Backend, alias: Option<&str>) -> String {
    let Some(alias) = alias else {
        return String::new();
    };
    match s.credentials.generic_delete(alias) {
        Ok(_) => String::new(),
        Err(_) => {
            audit(
                &s.state_dir,
                "setup-cleanup-failed",
                backend,
                &format!("credential={alias}"),
            );
            format!(
                "; the credential {alias} could not be removed: remove it with localpilot credential delete {alias}"
            )
        }
    }
}

// -------------------------------------------------------------- in session

/// Whether to offer setup now. Read fresh each time: research may use the
/// web, chat web access is not off, this is not an incognito session, no
/// search tool of any kind is configured, and the offer was not declined for
/// good.
#[cfg(feature = "tui")]
pub(crate) fn offer_wanted(root: &Path, incognito: bool) -> bool {
    if incognito {
        return false;
    }
    let Some(state) = state_dir() else {
        return false;
    };
    if declined(&state) {
        return false;
    }
    let Ok(config) = localpilot_config::load(
        &localpilot_config::ConfigPaths::standard(root),
        &localpilot_config::CliOverrides::default(),
    ) else {
        return false;
    };
    offer_fits(&config)
}

/// Whether `config` leaves web search with nothing to use while it may use
/// the web.
#[cfg(feature = "tui")]
fn offer_fits(config: &Config) -> bool {
    let (specs, unresolved) = crate::search_backend::search_specs(config);
    config.research.web.enabled
        && config.tools.web != localpilot_config::ChatWebMode::Off
        && specs.is_empty()
        && unresolved.is_empty()
        && config.search.managed.is_empty()
}

/// What setup shows in a session besides its questions.
#[cfg(feature = "tui")]
pub(crate) enum SetupNote {
    /// A few lines for the conversation.
    Notice(String),
    /// Lines to read in full before the next question (the backend list, the
    /// exact command and config): shown on a scrollable page of their own;
    /// `seen` is sent once that page is closed, and dropped if it never is.
    Review {
        lines: Vec<String>,
        seen: std::sync::mpsc::Sender<()>,
    },
}

/// More lines than this before a question are shown as a page to read first:
/// a question dialog covers part of the conversation on a small terminal.
#[cfg(feature = "tui")]
const REVIEW_AFTER_LINES: usize = 3;

/// Setup's questions as session dialogs, answered by the person at this
/// terminal. What setup says before a question is shown first, in full (a
/// page of its own when it is long), so the dialog holds only the question;
/// what it says after the last one is collected for the host to show. Only an
/// offered answer counts: a dismissal, typed text or a closed dialog cancels.
/// A key is never asked for here.
#[cfg(feature = "tui")]
pub(crate) struct DialogConsole {
    prompter: std::sync::Arc<dyn localpilot_tools::UserPrompter>,
    handle: tokio::runtime::Handle,
    notes: tokio::sync::mpsc::UnboundedSender<SetupNote>,
    lines: std::cell::RefCell<Vec<String>>,
}

#[cfg(feature = "tui")]
impl DialogConsole {
    pub(crate) fn new(
        prompter: std::sync::Arc<dyn localpilot_tools::UserPrompter>,
        handle: tokio::runtime::Handle,
        notes: tokio::sync::mpsc::UnboundedSender<SetupNote>,
    ) -> Self {
        Self {
            prompter,
            handle,
            notes,
            lines: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// What was said since the last question.
    pub(crate) fn take_lines(&self) -> Vec<String> {
        std::mem::take(&mut *self.lines.borrow_mut())
    }

    /// Ask on a session dialog; the index of the offered answer chosen. Must
    /// run on a thread of its own, never on a runtime worker.
    fn choose(&self, prompt: &str, labels: &[String]) -> Option<usize> {
        let said = self.take_lines();
        let first = said.iter().position(|line| !line.trim().is_empty());
        let last = said.iter().rposition(|line| !line.trim().is_empty());
        if let (Some(first), Some(last)) = (first, last) {
            let said = said[first..=last].to_vec();
            if said.len() > REVIEW_AFTER_LINES {
                // Read it all first; the question waits until the page is
                // closed, and is never asked if it is not.
                let (seen, read) = std::sync::mpsc::channel();
                if self
                    .notes
                    .send(SetupNote::Review { lines: said, seen })
                    .is_err()
                {
                    return None;
                }
                read.recv().ok()?;
            } else {
                let _ = self.notes.send(SetupNote::Notice(said.join("\n")));
            }
        }
        let question = localpilot_tools::UserQuestion {
            header: Some("Search setup".to_string()),
            question: prompt.to_string(),
            options: labels
                .iter()
                .map(|label| localpilot_tools::QuestionOption {
                    label: label.clone(),
                    description: None,
                })
                .collect(),
            multi_select: false,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let prompter = std::sync::Arc::clone(&self.prompter);
        self.handle.spawn(async move {
            let questions = [question];
            let _ = tx.send(prompter.ask(&questions).await);
        });
        let answers = rx.recv().ok()?;
        match answers.as_slice() {
            [localpilot_tools::UserAnswer::Selected(chosen)] => match chosen.as_slice() {
                [label] => labels.iter().position(|offered| offered == label),
                _ => None,
            },
            _ => None,
        }
    }
}

#[cfg(feature = "tui")]
impl SetupConsole for DialogConsole {
    fn say(&self, line: &str) {
        self.lines.borrow_mut().push(line.to_string());
    }

    fn pick(&self, prompt: &str, options: &[String]) -> Pick {
        let mut labels = options.to_vec();
        labels.push("None of these".to_string());
        match self.choose(prompt, &labels) {
            Some(index) if index < options.len() => Pick::Option(index),
            Some(_) => Pick::No,
            None => Pick::Cancel,
        }
    }

    fn ask(&self, prompt: &str) -> Answer {
        // "No" first: the dialog starts on the first choice, so Enter alone
        // never agrees to anything.
        match self.choose(prompt, &["No".to_string(), "Yes".to_string()]) {
            Some(1) => Answer::Yes,
            Some(_) => Answer::No,
            None => Answer::Cancel,
        }
    }

    fn secret(&self, _prompt: &str) -> Option<Secret> {
        None
    }
}

/// The offer made in a session that found no search backend: one question,
/// then (on yes) setup itself, with keys left to the terminal command. A
/// "don't offer again" is remembered until `localpilot search setup` runs.
/// Runs on a thread of its own; `stop` set means the session has gone.
#[cfg(feature = "tui")]
pub(crate) async fn offer_in_session(
    root: &Path,
    console: &DialogConsole,
    stop: &std::sync::atomic::AtomicBool,
) -> Outcome {
    let Some(state) = state_dir() else {
        return Outcome::Cancelled;
    };
    let options = [
        "Set one up".to_string(),
        "No, and don't offer again".to_string(),
    ];
    match console.choose(
        "No web search backend is set up, so web search and /research cannot search the web. Set one up now?",
        &options,
    ) {
        Some(0) => {}
        Some(_) => {
            match remember_decline(&state) {
                Ok(()) => console.say(
                    "LocalPilot will not offer this again. Run localpilot search setup to set one up.",
                ),
                Err(error) => console.say(&format!(
                    "Your answer could not be saved ({error}), so this may be offered again."
                )),
            }
            return Outcome::Declined;
        }
        None => return Outcome::Cancelled,
    }
    let stopped = || stop.load(std::sync::atomic::Ordering::SeqCst);
    if stopped() {
        return Outcome::Cancelled;
    }
    let credentials = CredentialStore::user();
    let effective = || {
        localpilot_config::load(
            &localpilot_config::ConfigPaths::standard(root),
            &localpilot_config::CliOverrides::default(),
        )
        .map_err(anyhow::Error::from)
    };
    let write = |path: Option<&Path>,
                 edit: &dyn Fn(&mut toml_edit::DocumentMut) -> Result<(), String>| {
        let fence = || {
            if stopped() {
                Err(STOPPED.to_string())
            } else {
                Ok(())
            }
        };
        crate::user_config::edit_user_config(path, &fence, &|_| {}, edit)
    };
    let setup = Setup {
        console,
        probes: &LocalProbes,
        credentials: &credentials,
        config_path: localpilot_config::user_config_path(),
        state_dir: state,
        launcher: crate::search_backend::spawn_launcher(),
        effective: &effective,
        random_suffix: &random_suffix,
        write: &write,
        keys: false,
        stopped: &stopped,
    };
    run(&setup).await
}

// ------------------------------------------------------------- production

/// `localpilot search setup`.
pub(crate) async fn run_command() -> anyhow::Result<()> {
    use std::io::IsTerminal as _;
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        anyhow::bail!(
            "localpilot search setup asks questions only a person at this terminal can answer; run it in a terminal"
        );
    }
    let state_dir =
        state_dir().ok_or_else(|| anyhow::anyhow!("no home folder for setup's records"))?;
    // Running setup by hand answers any earlier "no" to the in-session offer.
    clear_decline(&state_dir)?;
    let cwd = std::env::current_dir()?;
    let credentials = CredentialStore::user();
    let effective = || {
        localpilot_config::load(
            &localpilot_config::ConfigPaths::standard(&cwd),
            &localpilot_config::CliOverrides::default(),
        )
        .map_err(anyhow::Error::from)
    };
    let write = |path: Option<&Path>,
                 edit: &dyn Fn(&mut toml_edit::DocumentMut) -> Result<(), String>| {
        crate::user_config::edit_user_config(path, &|| Ok(()), &|_| {}, edit)
    };
    let setup = Setup {
        console: &Terminal,
        probes: &LocalProbes,
        credentials: &credentials,
        config_path: localpilot_config::user_config_path(),
        state_dir,
        launcher: crate::search_backend::spawn_launcher(),
        effective: &effective,
        random_suffix: &random_suffix,
        write: &write,
        keys: true,
        stopped: &|| false,
    };
    match run(&setup).await {
        Outcome::Configured(_) | Outcome::Declined | Outcome::Cancelled => Ok(()),
        Outcome::Stopped(reason) => Err(anyhow::anyhow!(reason)),
    }
}

/// `~/.localpilot/search`.
fn state_dir() -> Option<PathBuf> {
    crate::server_devices::user_devices_dir()
        .and_then(|devices| devices.parent().map(|home| home.join("search")))
}

fn random_suffix() -> Option<String> {
    use ring::rand::SecureRandom as _;
    let mut bytes = [0u8; 8];
    ring::rand::SystemRandom::new().fill(&mut bytes).ok()?;
    Some(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

struct Terminal;

fn read_line() -> Option<String> {
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim().to_string()),
    }
}

impl SetupConsole for Terminal {
    fn say(&self, line: &str) {
        println!("{line}");
    }

    fn pick(&self, prompt: &str, options: &[String]) -> Pick {
        use std::io::Write as _;
        println!("{prompt}");
        for (number, option) in options.iter().enumerate() {
            println!("  {}. {option}", number + 1);
        }
        println!("  n. None of these");
        print!("Choose a number, or n (Enter cancels): ");
        let _ = std::io::stdout().flush();
        let Some(answer) = read_line() else {
            return Pick::Cancel;
        };
        if answer.eq_ignore_ascii_case("n") || answer.eq_ignore_ascii_case("no") {
            return Pick::No;
        }
        answer
            .parse::<usize>()
            .ok()
            .and_then(|number| number.checked_sub(1))
            .filter(|index| *index < options.len())
            .map_or(Pick::Cancel, Pick::Option)
    }

    fn ask(&self, prompt: &str) -> Answer {
        use std::io::Write as _;
        print!("{prompt} [y/N] ");
        let _ = std::io::stdout().flush();
        match read_line()
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("y" | "yes") => Answer::Yes,
            Some("" | "n" | "no") => Answer::No,
            _ => Answer::Cancel,
        }
    }

    fn secret(&self, prompt: &str) -> Option<Secret> {
        use std::io::Write as _;
        print!("{prompt}");
        let _ = std::io::stdout().flush();
        // Nothing is shown while typing; an empty line cancels.
        let line = console::Term::stdout().read_secure_line().ok()?;
        let line = line.trim();
        (!line.is_empty()).then(|| Secret::new(line.to_string()))
    }
}

struct LocalProbes;

impl Probes for LocalProbes {
    fn find(&self, program: &str) -> Option<PathBuf> {
        resolve_in(
            program,
            &std::env::var_os("PATH")?,
            std::env::var("PATHEXT").ok().as_deref(),
        )
    }

    fn version(&self, program: &Path) -> Option<String> {
        let output = std::process::Command::new(program)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}

/// `program` in the absolute entries of `path_var`, trying each `pathext`
/// extension on Windows. Relative and empty entries (the working directory by
/// another name) are skipped.
fn resolve_in(program: &str, path_var: &std::ffi::OsStr, pathext: Option<&str>) -> Option<PathBuf> {
    let names: Vec<String> = if cfg!(windows) && Path::new(program).extension().is_none() {
        pathext
            .unwrap_or(".COM;.EXE;.BAT;.CMD")
            .split(';')
            .filter(|extension| !extension.is_empty())
            .map(|extension| format!("{program}{}", extension.to_ascii_lowercase()))
            .collect()
    } else {
        vec![program.to_string()]
    };
    std::env::split_paths(path_var)
        .filter(|directory| directory.is_absolute())
        .find_map(|directory| {
            names
                .iter()
                .map(|name| directory.join(name))
                .find(|candidate| candidate.is_file())
        })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::sync::Arc;

    struct Script {
        picks: RefCell<VecDeque<Pick>>,
        answers: RefCell<VecDeque<Answer>>,
        secret: Option<String>,
        said: RefCell<Vec<String>>,
        on_secret: Option<Box<dyn Fn()>>,
    }

    impl Script {
        fn new(pick: Pick, answers: &[Answer], secret: Option<&str>) -> Self {
            Self {
                picks: RefCell::new(VecDeque::from([pick])),
                answers: RefCell::new(answers.iter().copied().collect()),
                secret: secret.map(str::to_string),
                said: RefCell::new(Vec::new()),
                on_secret: None,
            }
        }
        fn text(&self) -> String {
            self.said.borrow().join("\n")
        }
    }

    impl SetupConsole for Script {
        fn say(&self, line: &str) {
            self.said.borrow_mut().push(line.to_string());
        }
        fn pick(&self, _prompt: &str, options: &[String]) -> Pick {
            self.said.borrow_mut().extend(options.iter().cloned());
            self.picks.borrow_mut().pop_front().unwrap_or(Pick::Cancel)
        }
        fn ask(&self, _prompt: &str) -> Answer {
            self.answers
                .borrow_mut()
                .pop_front()
                .unwrap_or(Answer::Cancel)
        }
        fn secret(&self, _prompt: &str) -> Option<Secret> {
            if let Some(hook) = &self.on_secret {
                hook();
            }
            self.secret.clone().map(Secret::new)
        }
    }

    struct Installed {
        uv: bool,
        node: Option<u32>,
    }

    impl Probes for Installed {
        fn find(&self, program: &str) -> Option<PathBuf> {
            let present = match program {
                "uvx" => self.uv,
                "node" | "npx" => self.node.is_some(),
                _ => false,
            };
            present.then(|| PathBuf::from(format!("/opt/tools/{program}")))
        }
        fn version(&self, program: &Path) -> Option<String> {
            if program.ends_with("node") {
                self.node.map(|major| format!("v{major}.1.0"))
            } else {
                Some("1.0.0".to_string())
            }
        }
    }

    const EVERYTHING: Installed = Installed {
        uv: true,
        node: Some(24),
    };

    fn working_launcher(tool: &str) -> Launcher {
        let transport = Arc::new(localpilot_mcp::ScriptedTransport::new().with(
            "tools/list",
            crate::search_backend::tests::search_tool_list(tool),
        ));
        crate::search_backend::tests::counting_launcher(transport).0
    }

    fn failing_launcher() -> Launcher {
        Arc::new(|_config, _handshake| Box::pin(async { Err("no".to_string()) }))
    }

    struct Home {
        _dir: tempfile::TempDir,
        config: PathBuf,
        state: PathBuf,
        credentials: CredentialStore,
    }

    fn home(config: &str) -> Home {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, config).unwrap();
        Home {
            config: config_path,
            state: dir.path().join("search"),
            credentials: CredentialStore::with_file(Some(dir.path().join("credentials.toml"))),
            _dir: dir,
        }
    }

    fn effective(path: &Path) -> anyhow::Result<Config> {
        Ok(localpilot_config::load(
            &localpilot_config::ConfigPaths {
                user: Some(path.to_path_buf()),
                project: None,
            },
            &localpilot_config::CliOverrides::default(),
        )?)
    }

    fn real_write(
        path: Option<&Path>,
        edit: &dyn Fn(&mut toml_edit::DocumentMut) -> Result<(), String>,
    ) -> Result<ConfigEdit, ConfigError> {
        crate::user_config::edit_user_config(path, &|| Ok(()), &|_| {}, edit)
    }

    async fn run_with(
        home: &Home,
        console: &Script,
        probes: &Installed,
        launcher: Launcher,
        write: ConfigWriter<'_>,
    ) -> Outcome {
        run_as(home, console, probes, launcher, write, true, &|| false).await
    }

    async fn run_as(
        home: &Home,
        console: &Script,
        probes: &Installed,
        launcher: Launcher,
        write: ConfigWriter<'_>,
        keys: bool,
        stopped: &dyn Fn() -> bool,
    ) -> Outcome {
        let config = home.config.clone();
        let read = move || effective(&config);
        let suffix = || Some("0123456789abcdef".to_string());
        let setup = Setup {
            console,
            probes,
            credentials: &home.credentials,
            config_path: Some(home.config.clone()),
            state_dir: home.state.clone(),
            launcher,
            effective: &read,
            random_suffix: &suffix,
            write,
            keys,
            stopped,
        };
        run(&setup).await
    }

    #[tokio::test]
    async fn keyless_setup_writes_one_verified_entry_and_records_each_step() {
        let home = home("# my settings\n[model]\nname = \"x\"\n");
        let console = Script::new(Pick::Option(0), &[Answer::Yes, Answer::No], None);
        let outcome = run_with(
            &home,
            &console,
            &EVERYTHING,
            working_launcher("search"),
            &real_write,
        )
        .await;
        assert_eq!(outcome, Outcome::Configured("duckduckgo".to_string()));
        let text = std::fs::read_to_string(&home.config).unwrap();
        assert!(
            text.starts_with("# my settings\n[model]\nname = \"x\"\n"),
            "{text}"
        );
        let config = effective(&home.config).unwrap();
        let entry = &config.search.managed["duckduckgo"];
        assert_eq!(
            entry.command,
            Path::new("/opt/tools/uvx").display().to_string()
        );
        assert_eq!(entry.args, vec!["--with", DDG_WITH_BROWSER, DDG_PACKAGE]);
        assert_eq!(
            entry.env["DDG_REF_URL_THRESHOLD"],
            McpEnvEntry::Plain("0".to_string())
        );
        assert_eq!(entry.tool, "search");
        assert_eq!(entry.call_timeout_secs, 45);
        assert!(entry.identity_matches());
        assert!(!entry.unattended, "unattended stays off unless asked for");
        let shown = console.text();
        assert!(shown.contains("run: /opt/tools/uvx --with \"duckduckgo-mcp-server[browser]==0.7.0\" duckduckgo-mcp-server==0.7.0"), "{shown}");
        assert!(
            shown.contains("[search.managed.duckduckgo]"),
            "the exact table is previewed: {shown}"
        );
        assert!(shown.contains("third-party code"), "{shown}");
        let audit = std::fs::read_to_string(audit_path(&home.state)).unwrap();
        let events: Vec<&str> = audit
            .lines()
            .map(|line| line.split(' ').nth(1).unwrap())
            .collect();
        assert_eq!(events, vec!["event=setup-start", "event=setup-done"]);
    }

    #[tokio::test]
    async fn no_or_no_answer_changes_nothing() {
        for (pick, answers, expected) in [
            (Pick::No, vec![], Outcome::Declined),
            (Pick::Cancel, vec![], Outcome::Cancelled),
            // A no at the second confirmation is not a no to search setup.
            (Pick::Option(0), vec![Answer::No], Outcome::Cancelled),
            (Pick::Option(0), vec![Answer::Cancel], Outcome::Cancelled),
        ] {
            let home = home("");
            let console = Script::new(pick, &answers, None);
            let outcome = run_with(
                &home,
                &console,
                &EVERYTHING,
                working_launcher("search"),
                &real_write,
            )
            .await;
            assert_eq!(outcome, expected);
            assert_eq!(std::fs::read_to_string(&home.config).unwrap(), "");
            assert!(
                !audit_path(&home.state).exists(),
                "nothing ran, nothing recorded"
            );
        }
    }

    #[tokio::test]
    async fn a_missing_prerequisite_stops_with_instructions() {
        let home = home("");
        let console = Script::new(Pick::Option(0), &[Answer::Yes], None);
        let missing = Installed {
            uv: false,
            node: Some(18),
        };
        let outcome = run_with(
            &home,
            &console,
            &missing,
            working_launcher("search"),
            &real_write,
        )
        .await;
        assert!(
            matches!(outcome, Outcome::Stopped(ref reason) if reason.contains("uv is not installed"))
        );
        let console = Script::new(Pick::Option(1), &[Answer::Yes], Some("key"));
        let outcome = run_with(
            &home,
            &console,
            &missing,
            working_launcher("brave_web_search"),
            &real_write,
        )
        .await;
        assert!(matches!(outcome, Outcome::Stopped(ref reason) if reason.contains("Node.js 22")));
        assert_eq!(std::fs::read_to_string(&home.config).unwrap(), "");
    }

    const OLD_BRAVE: &str = "[search.managed.brave]\ntier = \"api_key\"\ncommand = \"npx\"\nargs = [\"old\"]\ntool = \"brave_web_search\"\nlaunch_identity = \"sha256:old\"\n\n[search.managed.brave.env]\nBRAVE_API_KEY = { credential = \"search-brave-old\" }\n";
    const NEW_ALIAS: &str = "search-brave-0123456789abcdef";

    fn brave_home() -> Home {
        let home = home(OLD_BRAVE);
        home.credentials
            .generic_set("search-brave-old", &Secret::new("live-key"))
            .unwrap();
        home
    }

    fn assert_untouched(home: &Home, console: &Script) {
        assert_eq!(std::fs::read_to_string(&home.config).unwrap(), OLD_BRAVE);
        assert_eq!(
            home.credentials
                .generic_get("search-brave-old")
                .unwrap()
                .expose(),
            "live-key"
        );
        assert!(
            home.credentials.generic_get(NEW_ALIAS).is_none(),
            "the new alias is removed"
        );
        assert!(!console.text().contains("candidate-key"));
    }

    #[tokio::test]
    async fn a_failed_check_on_re_setup_keeps_the_old_entry_and_key() {
        let home = brave_home();
        let console = Script::new(Pick::Option(1), &[Answer::Yes], Some("candidate-key"));
        let outcome = run_with(
            &home,
            &console,
            &EVERYTHING,
            failing_launcher(),
            &real_write,
        )
        .await;
        assert!(matches!(outcome, Outcome::Stopped(_)), "{outcome:?}");
        assert_untouched(&home, &console);
        assert!(console.text().contains("replaces the brave backend"));
    }

    #[tokio::test]
    async fn a_refused_write_on_re_setup_keeps_the_old_entry_and_key() {
        let home = brave_home();
        let console = Script::new(
            Pick::Option(1),
            &[Answer::Yes, Answer::No],
            Some("candidate-key"),
        );
        let refused =
            |_: Option<&Path>, _: &dyn Fn(&mut toml_edit::DocumentMut) -> Result<(), String>| {
                Err(ConfigError::Before("busy".to_string()))
            };
        let outcome = run_with(
            &home,
            &console,
            &EVERYTHING,
            working_launcher("brave_web_search"),
            &refused,
        )
        .await;
        assert!(matches!(outcome, Outcome::Stopped(_)), "{outcome:?}");
        assert_untouched(&home, &console);
    }

    #[tokio::test]
    async fn an_unconfirmed_write_keeps_the_key_the_config_may_use() {
        let home = brave_home();
        let console = Script::new(
            Pick::Option(1),
            &[Answer::Yes, Answer::No],
            Some("candidate-key"),
        );
        let unconfirmed =
            |path: Option<&Path>,
             edit: &dyn Fn(&mut toml_edit::DocumentMut) -> Result<(), String>| {
                real_write(path, edit)?;
                Err(ConfigError::After("reads back differently".to_string()))
            };
        let outcome = run_with(
            &home,
            &console,
            &EVERYTHING,
            working_launcher("brave_web_search"),
            &unconfirmed,
        )
        .await;
        assert!(
            matches!(outcome, Outcome::Stopped(ref reason) if reason.contains(NEW_ALIAS) && reason.contains("was kept")),
            "{outcome:?}"
        );
        assert_eq!(
            home.credentials.generic_get(NEW_ALIAS).unwrap().expose(),
            "candidate-key"
        );
    }

    #[tokio::test]
    async fn a_keyed_setup_uses_a_fresh_alias_and_reports_the_old_one() {
        let home = brave_home();
        let console = Script::new(
            Pick::Option(1),
            &[Answer::Yes, Answer::Yes],
            Some("candidate-key"),
        );
        let outcome = run_with(
            &home,
            &console,
            &EVERYTHING,
            working_launcher("brave_web_search"),
            &real_write,
        )
        .await;
        assert_eq!(outcome, Outcome::Configured("brave".to_string()));
        let config = effective(&home.config).unwrap();
        let entry = &config.search.managed["brave"];
        assert_eq!(alias_of(entry), Some(NEW_ALIAS));
        assert!(entry.unattended);
        assert_eq!(entry.max_query_words, Some(50));
        assert!(entry
            .args
            .ends_with(&["--transport".to_string(), "stdio".to_string()]));
        let text = std::fs::read_to_string(&home.config).unwrap();
        assert!(
            !text.contains("candidate-key"),
            "the key never reaches the config: {text}"
        );
        assert_eq!(
            home.credentials
                .generic_get("search-brave-old")
                .unwrap()
                .expose(),
            "live-key"
        );
        let shown = console.text();
        assert!(
            shown.contains("search-brave-old is no longer used"),
            "{shown}"
        );
        assert!(!shown.contains("candidate-key"));
        assert!(!std::fs::read_to_string(audit_path(&home.state))
            .unwrap()
            .contains("candidate-key"));
    }

    #[tokio::test]
    async fn an_alias_that_already_exists_is_never_reused() {
        let home = brave_home();
        home.credentials
            .generic_set(NEW_ALIAS, &Secret::new("someone-elses"))
            .unwrap();
        let console = Script::new(Pick::Option(1), &[Answer::Yes], Some("candidate-key"));
        let outcome = run_with(
            &home,
            &console,
            &EVERYTHING,
            working_launcher("brave_web_search"),
            &real_write,
        )
        .await;
        assert!(
            matches!(outcome, Outcome::Stopped(ref reason) if reason.contains("already exists"))
        );
        assert_eq!(
            home.credentials.generic_get(NEW_ALIAS).unwrap().expose(),
            "someone-elses"
        );
        assert_eq!(std::fs::read_to_string(&home.config).unwrap(), OLD_BRAVE);
    }

    #[tokio::test]
    async fn cancelling_the_last_question_writes_nothing_and_removes_the_new_key() {
        let home = brave_home();
        let console = Script::new(
            Pick::Option(1),
            &[Answer::Yes, Answer::Cancel],
            Some("candidate-key"),
        );
        let outcome = run_with(
            &home,
            &console,
            &EVERYTHING,
            working_launcher("brave_web_search"),
            &real_write,
        )
        .await;
        assert_eq!(outcome, Outcome::Cancelled);
        assert_untouched(&home, &console);
        let audit = std::fs::read_to_string(audit_path(&home.state)).unwrap();
        assert!(
            audit.contains("event=setup-cancelled") && audit.contains("at=unattended"),
            "{audit}"
        );
    }

    #[tokio::test]
    async fn an_alias_taken_while_setup_waits_is_never_overwritten() {
        let home = brave_home();
        let mut console = Script::new(Pick::Option(1), &[Answer::Yes], Some("candidate-key"));
        let store = home.credentials.clone();
        console.on_secret = Some(Box::new(move || {
            store
                .generic_set(NEW_ALIAS, &Secret::new("arrived-meanwhile"))
                .unwrap();
        }));
        let outcome = run_with(
            &home,
            &console,
            &EVERYTHING,
            working_launcher("brave_web_search"),
            &real_write,
        )
        .await;
        assert!(
            matches!(outcome, Outcome::Stopped(ref reason) if reason.contains("appeared")),
            "{outcome:?}"
        );
        assert_eq!(
            home.credentials.generic_get(NEW_ALIAS).unwrap().expose(),
            "arrived-meanwhile"
        );
        assert_eq!(std::fs::read_to_string(&home.config).unwrap(), OLD_BRAVE);
    }

    #[tokio::test]
    async fn a_key_that_cannot_be_removed_is_named_and_recorded() {
        let home = brave_home();
        let console = Script::new(Pick::Option(1), &[Answer::Yes], Some("candidate-key"));
        // The check fails, and by then the credential file can no longer be
        // rewritten (a folder now stands where it was).
        let store_file = home._dir.path().join("credentials.toml");
        let launcher: Launcher = Arc::new(move |_config, _handshake| {
            let _ = std::fs::remove_file(&store_file);
            std::fs::create_dir_all(&store_file).unwrap();
            Box::pin(async { Err("no".to_string()) })
        });
        let outcome = run_with(&home, &console, &EVERYTHING, launcher, &real_write).await;
        assert!(
            matches!(outcome, Outcome::Stopped(ref reason)
                if reason.contains(&format!("localpilot credential delete {NEW_ALIAS}"))),
            "{outcome:?}"
        );
        let audit = std::fs::read_to_string(audit_path(&home.state)).unwrap();
        assert!(
            audit.contains(&format!(
                "event=setup-cleanup-failed id=brave tier=api_key credential={NEW_ALIAS}"
            )),
            "{audit}"
        );
        assert_eq!(std::fs::read_to_string(&home.config).unwrap(), OLD_BRAVE);
    }

    #[tokio::test]
    async fn without_a_terminal_a_key_backend_is_sent_to_the_terminal_command() {
        let home = brave_home();
        let console = Script::new(Pick::Option(1), &[Answer::Yes, Answer::No], Some("k"));
        let outcome = run_as(
            &home,
            &console,
            &EVERYTHING,
            working_launcher("brave_web_search"),
            &real_write,
            false,
            &|| false,
        )
        .await;
        assert!(
            matches!(outcome, Outcome::Stopped(ref reason) if reason.contains("localpilot search setup")),
            "{outcome:?}"
        );
        let said = console.said.borrow().join("\n");
        assert!(
            said.contains("Brave Search API, with your own key (set up in a terminal)"),
            "{said}"
        );
        assert!(!audit_path(&home.state).exists());
        assert!(home.credentials.generic_get(NEW_ALIAS).is_none());
        assert_eq!(std::fs::read_to_string(&home.config).unwrap(), OLD_BRAVE);
    }

    #[tokio::test]
    async fn a_session_that_goes_away_mid_setup_writes_nothing_and_removes_the_key() {
        let home = brave_home();
        let console = Script::new(
            Pick::Option(1),
            &[Answer::Yes, Answer::No],
            Some("candidate-key"),
        );
        let gone = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let launcher: Launcher = {
            let gone = Arc::clone(&gone);
            let inner = working_launcher("brave_web_search");
            Arc::new(move |config, handshake| {
                gone.store(true, std::sync::atomic::Ordering::SeqCst);
                inner(config, handshake)
            })
        };
        let stopped = || gone.load(std::sync::atomic::Ordering::SeqCst);
        let outcome = run_as(
            &home,
            &console,
            &EVERYTHING,
            launcher,
            &real_write,
            true,
            &stopped,
        )
        .await;
        assert_eq!(outcome, Outcome::Cancelled);
        assert_untouched(&home, &console);
        let audit = std::fs::read_to_string(audit_path(&home.state)).unwrap();
        assert!(
            audit.contains("event=setup-cancelled") && audit.contains("reason=stopped"),
            "{audit}"
        );
    }

    #[tokio::test]
    async fn a_stop_inside_the_config_lock_writes_nothing() {
        let home = home("");
        let console = Script::new(Pick::Option(0), &[Answer::Yes, Answer::No], None);
        let gone = std::sync::atomic::AtomicBool::new(false);
        let stopped = || gone.load(std::sync::atomic::Ordering::SeqCst);
        // The stop lands after setup's own last check, while the edit runs.
        let write =
            |path: Option<&Path>,
             edit: &dyn Fn(&mut toml_edit::DocumentMut) -> Result<(), String>| {
                gone.store(true, std::sync::atomic::Ordering::SeqCst);
                crate::user_config::edit_user_config(
                    path,
                    &|| {
                        if stopped() {
                            Err(STOPPED.to_string())
                        } else {
                            Ok(())
                        }
                    },
                    &|_| {},
                    edit,
                )
            };
        let outcome = run_as(
            &home,
            &console,
            &EVERYTHING,
            working_launcher("search"),
            &write,
            true,
            &stopped,
        )
        .await;
        assert_eq!(outcome, Outcome::Cancelled);
        assert_eq!(std::fs::read_to_string(&home.config).unwrap(), "");
    }

    #[cfg(feature = "tui")]
    #[test]
    fn the_offer_fits_only_when_web_search_has_nothing_to_use() {
        let mut config = Config::default();
        config.research.web.enabled = true;
        assert!(offer_fits(&config));
        let mut off = config.clone();
        off.tools.web = localpilot_config::ChatWebMode::Off;
        assert!(!offer_fits(&off));
        let mut no_web = config.clone();
        no_web.research.web.enabled = false;
        assert!(!offer_fits(&no_web));
        // Configured but not startable is still configured: no offer.
        let mut unresolved = config.clone();
        unresolved
            .research
            .mcp
            .tools
            .push(localpilot_config::ResearchMcpTool {
                server: "missing".to_string(),
                tool: "search".to_string(),
            });
        assert!(!offer_fits(&unresolved));
        assert!(!offer_wanted(Path::new("."), true));
    }

    #[cfg(feature = "tui")]
    #[test]
    fn a_no_to_the_offer_is_remembered_until_setup_runs_by_hand() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!declined(dir.path()));
        remember_decline(dir.path()).unwrap();
        assert!(declined(dir.path()));
        clear_decline(dir.path()).unwrap();
        assert!(!declined(dir.path()));
    }

    #[cfg(feature = "tui")]
    struct Answers(
        std::sync::Mutex<Vec<localpilot_tools::UserAnswer>>,
        std::sync::Mutex<Vec<String>>,
    );

    #[cfg(feature = "tui")]
    impl localpilot_tools::UserPrompter for Answers {
        fn ask<'a>(
            &'a self,
            questions: &'a [localpilot_tools::UserQuestion],
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Vec<localpilot_tools::UserAnswer>> + Send + 'a>,
        > {
            self.1.lock().unwrap().push(format!(
                "{}|{}",
                questions[0].question, questions[0].options[0].label
            ));
            let answer = self.0.lock().unwrap().remove(0);
            Box::pin(async move { vec![answer] })
        }
    }

    #[cfg(feature = "tui")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_dialogs_count_only_an_offered_answer() {
        use localpilot_tools::UserAnswer;
        let picked = |label: &str| UserAnswer::Selected(vec![label.to_string()]);
        let prompter = Arc::new(Answers(
            std::sync::Mutex::new(vec![
                picked("Brave Search API"),
                picked("None of these"),
                UserAnswer::Other("duckduckgo".to_string()),
                UserAnswer::Dismissed,
                picked("Yes"),
                picked("No"),
                UserAnswer::Other("yes".to_string()),
            ]),
            std::sync::Mutex::new(Vec::new()),
        ));
        let handle = tokio::runtime::Handle::current();
        let asked = Arc::clone(&prompter);
        let (notice_tx, mut notices) = tokio::sync::mpsc::unbounded_channel::<SetupNote>();
        let answers = tokio::task::spawn_blocking(move || {
            let console = DialogConsole::new(prompter, handle, notice_tx);
            let options = vec!["DuckDuckGo".to_string(), "Brave Search API".to_string()];
            console.say("Tier 1, no key:");
            let picks: Vec<Pick> = (0..4).map(|_| console.pick("Which?", &options)).collect();
            let asks: Vec<Answer> = (0..3).map(|_| console.ask("Sure?")).collect();
            console.say("Done.");
            (picks, asks, console.secret("key"), console.take_lines())
        })
        .await
        .unwrap();
        assert_eq!(
            answers.0,
            vec![Pick::Option(1), Pick::No, Pick::Cancel, Pick::Cancel]
        );
        assert_eq!(answers.1, vec![Answer::Yes, Answer::No, Answer::Cancel]);
        assert!(answers.2.is_none());
        assert_eq!(answers.3, vec!["Done.".to_string()]);
        let texts = asked.1.lock().unwrap().clone();
        assert_eq!(texts[0], "Which?|DuckDuckGo");
        // A yes/no starts on "No", so Enter alone agrees to nothing; what a
        // question means is shown before it, so the question stays short.
        assert_eq!(texts[4], "Sure?|No");
        assert!(texts.iter().all(|text| text.len() < 160), "{texts:?}");
        assert!(
            matches!(notices.try_recv().unwrap(), SetupNote::Notice(text) if text == "Tier 1, no key:")
        );
        assert!(notices.try_recv().is_err());
    }

    #[cfg(feature = "tui")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_long_preview_is_read_in_full_before_its_question() {
        use localpilot_tools::UserAnswer;
        let prompter = Arc::new(Answers(
            std::sync::Mutex::new(vec![UserAnswer::Selected(vec!["Yes".to_string()])]),
            std::sync::Mutex::new(Vec::new()),
        ));
        let asked = Arc::clone(&prompter);
        let handle = tokio::runtime::Handle::current();
        let (notes_tx, mut notes) = tokio::sync::mpsc::unbounded_channel::<SetupNote>();
        let ask = tokio::task::spawn_blocking(move || {
            let console = DialogConsole::new(prompter, handle, notes_tx);
            for line in [
                "Setup will:",
                "  run: uvx",
                "  write to config.toml:",
                "    [search.managed.duckduckgo]",
            ] {
                console.say(line);
            }
            console.ask("Set this up now?")
        });
        let Some(SetupNote::Review { lines, seen }) = notes.recv().await else {
            panic!("expected a page to read first");
        };
        assert_eq!(lines.len(), 4);
        // Not asked while the page is open.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(asked.1.lock().unwrap().is_empty());
        seen.send(()).unwrap();
        assert_eq!(ask.await.unwrap(), Answer::Yes);
        assert_eq!(asked.1.lock().unwrap().len(), 1);

        // A page never closed (the session went away) asks nothing.
        let prompter = Arc::new(Answers(
            std::sync::Mutex::new(Vec::new()),
            std::sync::Mutex::new(Vec::new()),
        ));
        let handle = tokio::runtime::Handle::current();
        let (notes_tx, mut notes) = tokio::sync::mpsc::unbounded_channel::<SetupNote>();
        let ask = tokio::task::spawn_blocking(move || {
            let console = DialogConsole::new(prompter, handle, notes_tx);
            for line in ["a", "b", "c", "d"] {
                console.say(line);
            }
            console.ask("Set this up now?")
        });
        drop(notes.recv().await);
        drop(notes);
        assert_eq!(ask.await.unwrap(), Answer::Cancel);
    }

    #[test]
    fn programs_are_found_only_in_absolute_path_entries() {
        let dir = tempfile::tempdir().unwrap();
        let tools = dir.path().join("tools");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        let name = if cfg!(windows) { "uvx.exe" } else { "uvx" };
        std::fs::write(workspace.join(name), "").unwrap();
        // A relative entry and an empty one name the working directory; a
        // workspace file there must not be chosen, even if listed first.
        let path = std::env::join_paths([
            PathBuf::from("."),
            PathBuf::new(),
            PathBuf::from("workspace"),
            tools.clone(),
        ])
        .unwrap();
        assert_eq!(resolve_in("uvx", &path, Some(".EXE;.CMD")), None);
        std::fs::write(tools.join(name), "").unwrap();
        assert_eq!(
            resolve_in("uvx", &path, Some(".EXE;.CMD")),
            Some(tools.join(name))
        );
    }

    #[cfg(windows)]
    #[test]
    fn on_windows_a_command_script_is_found_by_its_extension() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("npx.cmd"), "").unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(
            resolve_in("npx", &path, Some(".COM;.EXE;.BAT;.CMD")),
            Some(dir.path().join("npx.cmd"))
        );
    }

    #[test]
    fn running_setup_by_hand_clears_an_earlier_no() {
        let dir = tempfile::tempdir().unwrap();
        clear_decline(dir.path()).unwrap();
        std::fs::write(decline_marker(dir.path()), "").unwrap();
        clear_decline(dir.path()).unwrap();
        assert!(!decline_marker(dir.path()).exists());
    }
}
