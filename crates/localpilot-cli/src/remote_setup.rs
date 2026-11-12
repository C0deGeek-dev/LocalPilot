//! `localpilot remote setup`: give the phone an HTTPS address through
//! Tailscale on this computer, changing only what setup adds and recording
//! only what it can confirm.
//!
//! A run reads first and refuses before any change when anything is unclear,
//! then asks. After consent every write is preceded by a fresh, fully
//! validated read (same node and tailnet, a stable release setup may change,
//! settings it understands, a version tag), so nothing is written on the
//! strength of what was seen before the prompt. In order:
//! 1. a test save Tailscale must refuse (a deliberately stale version tag),
//!    which shows its conditional write works on this install;
//! 2. optionally, a random name for this computer, offered because the
//!    certificate publishes the name;
//! 3. the phone origin, written only on the settings it was planned on;
//! 4. the gateway address in the user config;
//! 5. checks of the gateway and of the address from this computer.
//!
//! The journal keeps what LocalPilot has confirmed it owns, every operation
//! whose outcome is not confirmed, and the newest result, each on its own:
//! an unconfirmed operation blocks further changes until the user resolves
//! it, and no later result erases an earlier record.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::remote_gateway::{Listener, OriginAnswer};
use crate::remote_tailscale::{
    eligible, name_in_use, one_label, origin_plan, read_node, schema_ok, supported, version_tag,
    without_ours, ApiFailure, Node, OriginPlan, Refusal, Reply,
};

const STATUS: &str = "/localapi/v0/status?peers=false";
const SERVE: &str = "/localapi/v0/serve-config";
const PREFS: &str = "/localapi/v0/prefs";
/// How long a new name may take to reach this computer's status.
pub(crate) const SETTLE_WITHIN: Duration = Duration::from_secs(30);
const SETTLE_EVERY: Duration = Duration::from_secs(1);
const JOURNAL_VERSION: u32 = 2;
/// How long one request of the address check may take. The first HTTPS
/// connection to a new name waits while Tailscale gets its certificate.
const TLS_CHECK_WITHIN: Duration = Duration::from_secs(90);

/// The daemon's local API, as setup uses it.
#[async_trait::async_trait]
pub(crate) trait Daemon: Send + Sync {
    async fn get(&self, path: &str) -> Result<Reply, ApiFailure>;
    async fn post(&self, path: &str, body: &Value, if_match: &str) -> Result<Reply, ApiFailure>;
    async fn patch(&self, path: &str, body: &Value) -> Result<Reply, ApiFailure>;
}

/// The Tailscale daemon on this computer.
pub(crate) struct LocalDaemon;

#[async_trait::async_trait]
impl Daemon for LocalDaemon {
    async fn get(&self, path: &str) -> Result<Reply, ApiFailure> {
        crate::remote_tailscale::get(path).await
    }
    async fn post(&self, path: &str, body: &Value, if_match: &str) -> Result<Reply, ApiFailure> {
        crate::remote_tailscale::post_json(path, body, if_match).await
    }
    async fn patch(&self, path: &str, body: &Value) -> Result<Reply, ApiFailure> {
        crate::remote_tailscale::patch_json(path, body).await
    }
}

/// The checks of what listens on the gateway port and of the address.
#[async_trait::async_trait]
pub(crate) trait Probes: Send + Sync {
    async fn listener(&self, port: u16) -> Listener;
    async fn origin(&self, port: u16, host: &str) -> OriginAnswer;
    /// The address from this computer over real TLS: `Ok` only for the
    /// gateway's health answer and its exact missing-credential refusal.
    async fn tls(&self, origin: &str) -> Result<(), String>;
}

/// What the person running setup sees and answers.
pub(crate) trait Console: Send + Sync {
    fn say(&self, line: &str);
    /// Ask with `choices`; anything but a clear pick is [`Choice::Cancel`].
    fn choose(&self, prompt: &str, choices: &[Choice]) -> Choice;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Choice {
    RandomName,
    KeepName,
    Proceed,
    Cancel,
}

impl Choice {
    fn label(self) -> &'static str {
        match self {
            Self::RandomName => "Use the random name",
            Self::KeepName => "Keep the current name",
            Self::Proceed => "Go ahead",
            Self::Cancel => "Cancel",
        }
    }
}

/// What the command line asked for about the name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NameFlag {
    Ask,
    Random,
    Keep,
}

/// Fills a buffer from the operating system's random source.
pub(crate) type Random = dyn Fn(&mut [u8]) -> Result<(), String> + Sync;

/// Sets the phone address in the user config.
pub(crate) type EditConfig = dyn Fn(Option<&Path>, &str) -> Result<ConfigEdit, ConfigError> + Sync;

/// Everything a run touches, so every path can be exercised with fakes.
pub(crate) struct Setup<'a> {
    pub daemon: &'a dyn Daemon,
    pub probes: &'a dyn Probes,
    pub console: &'a dyn Console,
    /// The number of devices paired and not revoked, from the whole registry.
    pub devices: &'a (dyn Fn() -> Result<usize, String> + Sync),
    /// Where the journal and its lock live.
    pub state_dir: PathBuf,
    /// The user config file.
    pub config_path: Option<PathBuf>,
    pub port: u16,
    pub interactive: bool,
    pub settle_within: Duration,
    /// Writes the journal; replaced in tests to fail on purpose.
    pub persist: &'a (dyn Fn(&Path, &Journal) -> std::io::Result<()> + Sync),
    /// Edits the user config; replaced in tests.
    pub edit_config: &'a EditConfig,
    pub random: &'a Random,
}

/// Why a command stopped, as the person running it reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stop(pub String);

impl std::fmt::Display for Stop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a step stopped, before the run adds what was already done.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Halt {
    /// This step changed nothing.
    Clean(String),
    /// Whether this step changed something is not known; the message says
    /// what to check.
    Unknown(String),
}

type Step<T> = Result<T, Halt>;

fn halt<T>(message: impl Into<String>) -> Step<T> {
    Err(Halt::Clean(message.into()))
}

fn unknown<T>(message: impl Into<String>) -> Step<T> {
    Err(Halt::Unknown(message.into()))
}

fn refused<T>(refusal: &Refusal) -> Step<T> {
    let remedy = refusal.remedy();
    let remedy = remedy
        .strip_prefix("Nothing was changed. ")
        .unwrap_or(remedy);
    halt(format!("{}. {remedy}", refusal.message()))
}

/// The final message: the reason, then what was done before it.
fn finish(halted: Halt, done: &[String]) -> Stop {
    let before = done.join("; ");
    Stop(match halted {
        Halt::Clean(message) if done.is_empty() => format!("{message} Nothing was changed."),
        Halt::Clean(message) => {
            format!("{message} Done before this: {before}. Nothing else was changed.")
        }
        Halt::Unknown(message) if done.is_empty() => message,
        Halt::Unknown(message) => format!("{message} Done before this: {before}."),
    })
}

// ---------------------------------------------------------------- journal

/// What setup knows about its own work on this computer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Journal {
    pub version: u32,
    /// Every phone origin LocalPilot added and confirmed.
    #[serde(default)]
    pub serve: Vec<ServeOwned>,
    /// The name LocalPilot gave this computer and confirmed.
    #[serde(default)]
    pub rename: Option<RenameOwned>,
    /// Operations whose outcome is not confirmed (planned and cut off,
    /// unknown, or a test save that was accepted). Each stays until the user
    /// resolves it; none confers ownership.
    #[serde(default)]
    pub pending: Vec<Attempt>,
    /// The newest operation's result, whatever it was.
    #[serde(default)]
    pub last: Option<Attempt>,
}

impl Default for Journal {
    fn default() -> Self {
        Self {
            version: JOURNAL_VERSION,
            serve: Vec::new(),
            rename: None,
            pending: Vec::new(),
            last: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ServeOwned {
    pub node_id: String,
    pub tailnet: String,
    /// The machine name the origin is on.
    pub name: String,
    pub port: u16,
    /// Checked over real TLS from this computer.
    pub verified: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RenameOwned {
    pub node_id: String,
    pub tailnet: String,
    /// Tailscale's name setting before setup changed it; empty means "use
    /// the computer's own host name".
    pub old_hostname: String,
    pub new_hostname: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Attempt {
    pub step: Op,
    pub state: State,
    pub node_id: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub old_hostname: Option<String>,
    #[serde(default)]
    pub new_hostname: Option<String>,
}

impl Attempt {
    fn new(step: Op, node: &Node) -> Self {
        Self {
            step,
            state: State::Planned,
            node_id: node.node_id.clone(),
            detail: String::new(),
            old_hostname: None,
            new_hostname: None,
        }
    }

    fn describe(&self) -> String {
        let what = match self.step {
            Op::Probe => "a test save",
            Op::Rename => "renaming this computer",
            Op::Serve => "adding the phone address",
            Op::UndoServe => "removing the phone address",
            Op::UndoRename => "restoring the name setting",
        };
        let names = match (&self.old_hostname, &self.new_hostname) {
            (Some(old), Some(new)) => {
                let old = if old.is_empty() {
                    "the computer's own host name"
                } else {
                    old
                };
                format!(" (from {old} to {new})")
            }
            _ => String::new(),
        };
        let detail = if self.detail.is_empty() {
            "it was cut off before its result was known".to_string()
        } else {
            self.detail.clone()
        };
        format!("{what}{names}: {detail}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Op {
    Probe,
    Rename,
    Serve,
    UndoServe,
    UndoRename,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum State {
    /// Written before the request; still there means the run was cut off.
    Planned,
    /// The test save was refused as it must be.
    Passed,
    /// Confirmed by a fresh read.
    Done,
    /// A definite refusal: nothing was changed.
    Refused,
    /// The outcome is not known.
    Unresolved,
    /// The test save was accepted: this install does not protect settings.
    Unexpected,
}

fn journal_path(dir: &Path) -> PathBuf {
    dir.join("setup.json")
}

fn load_journal(dir: &Path) -> Step<Journal> {
    let path = journal_path(dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Journal::default()),
        Err(error) => return halt(format!("Could not read {}: {error}.", path.display())),
    };
    let journal: Journal = serde_json::from_str(&text).map_err(|error| {
        Halt::Clean(format!(
            "{} is not a record this version understands ({error}). Move it aside only after \
             checking your Tailscale settings.",
            path.display()
        ))
    })?;
    if journal.version != JOURNAL_VERSION {
        return halt(format!(
            "{} was written by another version of LocalPilot.",
            path.display()
        ));
    }
    Ok(journal)
}

/// Write the journal atomically, readable only by this user: the folder and
/// the temporary file are restricted before anything is written, whatever
/// permissions an earlier file had.
pub(crate) fn write_journal(dir: &Path, journal: &Journal) -> std::io::Result<()> {
    use std::io::Write as _;
    std::fs::create_dir_all(dir)?;
    crate::server_devices::restrict(dir)?;
    let text = serde_json::to_vec_pretty(journal).map_err(std::io::Error::other)?;
    let temporary = dir.join("setup.json.tmp");
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        crate::server_devices::restrict(&temporary)?;
        file.write_all(&text)?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, journal_path(dir))
}

/// The run's exclusive hold on setup.
struct Held {
    _lock: Box<fd_lock::RwLock<std::fs::File>>,
}

fn hold(dir: &Path) -> Step<Held> {
    std::fs::create_dir_all(dir)
        .and_then(|()| crate::server_devices::restrict(dir))
        .map_err(|error| Halt::Clean(format!("Could not prepare {}: {error}.", dir.display())))?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("setup.lock"))
        .map_err(|error| Halt::Clean(format!("Could not open the setup lock: {error}.")))?;
    let mut lock = Box::new(fd_lock::RwLock::new(file));
    // The guard is forgotten so the lock stays taken until `Held` closes the
    // file at the end of the run.
    match lock.try_write() {
        Ok(guard) => std::mem::forget(guard),
        Err(_) => return halt("Another `localpilot remote setup` is running."),
    }
    Ok(Held { _lock: lock })
}

/// One run's journal and progress.
struct Run<'a, 's> {
    s: &'a Setup<'s>,
    journal: Journal,
    /// What this run confirmed it changed, in order.
    done: Vec<String>,
}

impl Run<'_, '_> {
    fn save(&self) -> Result<(), String> {
        (self.s.persist)(&self.s.state_dir, &self.journal).map_err(|error| error.to_string())
    }

    /// Record `attempt` as pending before acting on it. Failing to record
    /// means not acting.
    fn plan(&mut self, attempt: Attempt) -> Step<()> {
        self.journal.pending.push(attempt.clone());
        self.journal.last = Some(attempt);
        if let Err(error) = self.save() {
            self.journal.pending.pop();
            return halt(format!(
                "LocalPilot could not record what it was about to do ({error})."
            ));
        }
        Ok(())
    }

    /// Settle the newest attempt: a definite result leaves the pending list,
    /// an unknown one stays on it.
    fn settle(&mut self, state: State, detail: &str) -> Result<(), String> {
        if let Some(last) = self.journal.last.as_mut() {
            last.state = state;
            last.detail = detail.to_string();
        }
        if let Some(pending) = self.journal.pending.last_mut() {
            pending.state = state;
            pending.detail = detail.to_string();
        }
        if matches!(state, State::Passed | State::Done | State::Refused) {
            self.journal.pending.pop();
        }
        self.save()
    }

    fn random_bytes(&self, buffer: &mut [u8]) -> Step<()> {
        (self.s.random)(buffer).map_err(|error| {
            Halt::Clean(format!("This computer's random source failed ({error})."))
        })
    }

    fn random_hex(&self, bytes: usize) -> Step<String> {
        let mut buffer = vec![0u8; bytes];
        self.random_bytes(&mut buffer)?;
        Ok(buffer.iter().fold(String::new(), |mut text, byte| {
            use std::fmt::Write as _;
            let _ = write!(text, "{byte:02x}");
            text
        }))
    }

    /// A name with nothing of LocalPilot in it: the shape Windows gives a new
    /// computer (`DESKTOP-` and seven letters or digits, lower-cased as
    /// MagicDNS shows it), so in public certificate logs the machine part is
    /// one of very many and says nothing about what runs there.
    fn random_name(&self) -> Step<String> {
        const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
        // Bytes at or above this would favour some characters.
        const FAIR: u8 = 252;
        let mut label = String::new();
        while label.len() < 7 {
            let mut buffer = [0u8; 16];
            self.random_bytes(&mut buffer)?;
            for byte in buffer {
                if byte < FAIR && label.len() < 7 {
                    label.push(ALPHABET[usize::from(byte) % ALPHABET.len()] as char);
                }
            }
        }
        Ok(format!("desktop-{label}"))
    }
}

// ------------------------------------------------------------- reading

/// One reading of the daemon.
struct Reading {
    status: Value,
    node: Node,
    config: Value,
    etag: Option<String>,
}

async fn document(daemon: &dyn Daemon, path: &str, what: &str) -> Step<(Value, Option<String>)> {
    let reply = daemon
        .get(path)
        .await
        .map_err(|failure| Halt::Clean(format!("{failure}.")))?;
    match reply.status {
        200 => {}
        403 => {
            return halt(format!(
                "This user may not read Tailscale's {what}. On Linux, `sudo tailscale set \
                 --operator=$USER` lets your user manage Tailscale."
            ))
        }
        status => {
            let detail: String = String::from_utf8_lossy(&reply.body)
                .trim()
                .chars()
                .take(200)
                .collect();
            return halt(format!(
                "Tailscale answered {status} when asked for its {what}: {detail}."
            ));
        }
    }
    if reply.truncated {
        return halt(format!("Tailscale's {what} was larger than setup reads."));
    }
    let value = serde_json::from_slice(&reply.body)
        .map_err(|error| Halt::Clean(format!("Tailscale's {what} was not JSON ({error}).")))?;
    Ok((value, reply.etag))
}

async fn read(daemon: &dyn Daemon) -> Step<Reading> {
    let (status, _) = document(daemon, STATUS, "status").await?;
    let node = read_node(&status).or_else(|refusal| refused(&refusal))?;
    let (config, etag) = document(daemon, SERVE, "Serve settings").await?;
    Ok(Reading {
        status,
        node,
        config,
        etag,
    })
}

async fn hostname(daemon: &dyn Daemon) -> Step<String> {
    let (prefs, _) = document(daemon, PREFS, "preferences").await?;
    prefs
        .get("Hostname")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| Halt::Clean("Tailscale's preferences have no name setting.".to_string()))
}

fn same_node(a: &Node, node_id: &str, tailnet: &str) -> bool {
    a.node_id == node_id && a.tailnet == tailnet
}

/// A fresh reading that is safe to write on: the same node and tailnet as
/// `who`, a stable release setup may change, connected, settings it
/// understands and a version tag. Taken immediately before every write.
async fn fresh(daemon: &dyn Daemon, who: &Node) -> Step<Reading> {
    let reading = read(daemon).await?;
    if !same_node(&reading.node, &who.node_id, &who.tailnet) {
        return halt(
            "Tailscale is now signed in as another node or tailnet than when setup started.",
        );
    }
    if !supported(&reading.node.version) {
        return refused(&Refusal::Unsupported(reading.node.version));
    }
    if reading.node.state != "Running" {
        return refused(&Refusal::NotRunning(reading.node.state));
    }
    schema_ok(&reading.config).or_else(|refusal| refused(&refusal))?;
    version_tag(reading.etag.as_deref()).or_else(|refusal| refused(&refusal))?;
    Ok(reading)
}

fn expect_name(reading: &Reading, name: &str) -> Step<()> {
    if reading.node.name.eq_ignore_ascii_case(name) {
        Ok(())
    } else {
        halt(format!(
            "This computer's Tailscale name is now {} instead of {name}.",
            reading.node.name
        ))
    }
}

/// Refuse while an earlier operation's outcome is unconfirmed.
fn no_pending(journal: &Journal) -> Step<()> {
    if journal.pending.is_empty() {
        return Ok(());
    }
    let list: Vec<String> = journal.pending.iter().map(Attempt::describe).collect();
    halt(format!(
        "An earlier run did not confirm what happened: {}. Check `tailscale status` and \
         `tailscale serve status`, then run `localpilot remote setup --resolve` to confirm you \
         have looked; LocalPilot will not treat any of it as its own.",
        list.join("; ")
    ))
}

/// The node and tailnet every confirmed record was made on.
fn owners(journal: &Journal) -> Vec<(String, String)> {
    journal
        .serve
        .iter()
        .map(|o| (o.node_id.clone(), o.tailnet.clone()))
        .chain(
            journal
                .rename
                .iter()
                .map(|o| (o.node_id.clone(), o.tailnet.clone())),
        )
        .collect()
}

// ------------------------------------------------------------- check

/// `localpilot remote setup --check`: what setup would do, by GET only.
pub(crate) async fn check(
    daemon: &dyn Daemon,
    console: &dyn Console,
    port: u16,
) -> Result<(), Stop> {
    let inner = async {
        let (status, _) = document(daemon, STATUS, "status").await?;
        let (config, etag) = document(daemon, SERVE, "Serve settings").await?;
        if let Ok(node) = read_node(&status) {
            console.say(&format!(
                "Tailscale {} on {} (tailnet {}), {}.",
                node.version,
                if node.name.is_empty() {
                    "this computer"
                } else {
                    &node.name
                },
                node.tailnet,
                node.state
            ));
        }
        match crate::remote_tailscale::assess(&status, &config, etag.as_deref(), port) {
            crate::remote_tailscale::Readiness::Ready(change) => {
                console.say(&format!(
                    "Setup would serve {}/ from the gateway on 127.0.0.1:{port}, changing \
                     nothing else in your Serve settings.",
                    change.origin
                ));
                if let Some(note) = &change.admin_note {
                    console.say(&format!("Note: {note}."));
                }
                console.say(&format!(
                    "Serving it publishes the name {} in public certificate logs; setup offers a \
                     random name for this computer first.",
                    change.origin.trim_start_matches("https://")
                ));
                console
                    .say("This check changed nothing. Run `localpilot remote setup` to set it up.");
                Ok(())
            }
            crate::remote_tailscale::Readiness::AlreadyServed { origin } => {
                console.say(&format!(
                    "{origin}/ already forwards to the gateway on 127.0.0.1:{port}."
                ));
                Ok(())
            }
            crate::remote_tailscale::Readiness::Refused(refusal) => refused(&refusal),
        }
    };
    inner.await.map_err(|halted| finish(halted, &[]))
}

// ------------------------------------------------------------- setup

/// The limit of the test save, stated before it is sent.
const PROBE_LIMIT: &str = "First, setup sends a test save that Tailscale must refuse, to \
     confirm it never saves over settings that changed in between. If this install ignored \
     that check, the test save would write back your settings as they were just read, which \
     could undo a change made elsewhere at that same moment.";

/// The disclosure shown before any change, the same whether it is answered
/// at a prompt or given by flags.
fn disclosure(
    current: &str,
    proposed: Option<&str>,
    suffix: &str,
    devices: usize,
    port: u16,
) -> Vec<String> {
    let published = proposed.unwrap_or(current);
    let mut lines = vec![
        format!(
            "Setup will serve https://{published}/ from the LocalPilot gateway on \
             127.0.0.1:{port}, to devices in your tailnet only."
        ),
        format!(
            "Its HTTPS certificate puts the name {published} permanently in public \
             Certificate Transparency logs, including the tailnet part {suffix}, which stays \
             as it is. Names in earlier certificates cannot be removed."
        ),
        PROBE_LIMIT.to_string(),
    ];
    if let Some(proposed) = proposed {
        lines.push(format!(
            "Renaming this computer in Tailscale from {current} to {proposed} changes its \
             name for every device and app in your tailnet that uses it, which can break \
             things outside LocalPilot (SSH, file shares, bookmarks)."
        ));
        if devices > 0 {
            lines.push(format!(
                "Your {devices} paired device(s) will have to pair again."
            ));
        }
        lines.push(
            "Tailscale cannot make a name change conditional: a change made to the name \
             elsewhere at the same moment could be overwritten."
                .to_string(),
        );
        lines.push(
            "The new machine name says nothing about you or LocalPilot; the tailnet part of \
             the address is unchanged, and nothing makes the address anonymous."
                .to_string(),
        );
    }
    lines
}

/// Show `lines`, then go ahead only with `--yes` or a yes at the prompt.
fn consent(s: &Setup<'_>, lines: &[String], yes: bool) -> Step<()> {
    for line in lines {
        s.console.say(line);
    }
    if yes {
        return Ok(());
    }
    if !s.interactive {
        return halt("Without a terminal to ask in, add --yes to confirm.");
    }
    if s.console
        .choose("Go ahead?", &[Choice::Proceed, Choice::Cancel])
        == Choice::Proceed
    {
        Ok(())
    } else {
        halt("Cancelled.")
    }
}

/// Whether a rename can be offered, and if not, why.
enum Rename {
    Offer { proposed: String, devices: usize },
    No(String),
}

/// Take the lock and the journal, run `body`, and turn its stop into the
/// final message with what was done before it.
async fn with_journal<F>(s: &Setup<'_>, body: F) -> Result<(), Stop>
where
    F: for<'r, 'a, 'b> FnOnce(
        &'r mut Run<'a, 'b>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Step<()>> + 'r>,
    >,
{
    let held = hold(&s.state_dir).map_err(|halted| finish(halted, &[]))?;
    let journal = load_journal(&s.state_dir).map_err(|halted| finish(halted, &[]))?;
    let mut run = Run {
        s,
        journal,
        done: Vec::new(),
    };
    let result = body(&mut run).await;
    drop(held);
    result.map_err(|halted| finish(halted, &run.done))
}

/// `localpilot remote setup`.
pub(crate) async fn run(s: &Setup<'_>, flag: NameFlag, yes: bool) -> Result<(), Stop> {
    with_journal(s, |run| Box::pin(setup(run, flag, yes))).await
}

async fn setup(run: &mut Run<'_, '_>, flag: NameFlag, yes: bool) -> Step<()> {
    let s = run.s;
    no_pending(&run.journal)?;

    // Read, and refuse before asking when anything is in the way.
    let start = read(s.daemon).await?;
    let node = eligible(&start.status).or_else(|refusal| refused(&refusal))?;
    version_tag(start.etag.as_deref()).or_else(|refusal| refused(&refusal))?;
    origin_plan(&start.config, &node.name, s.port).or_else(|refusal| refused(&refusal))?;
    let start_hostname = hostname(s.daemon).await?;
    for (node_id, tailnet) in owners(&run.journal) {
        if !same_node(&node, &node_id, &tailnet) {
            return halt(
                "LocalPilot's earlier setup was made on another Tailscale node or tailnet. Run \
                 `localpilot remote setup --undo` while signed in as that one first.",
            );
        }
    }

    let rename = if run.journal.rename.is_some() {
        Rename::No("this computer already has the name LocalPilot gave it".to_string())
    } else if let Some(use_) = name_in_use(&start.config, &node.name) {
        Rename::No(format!("{use_}, and a new name would move it"))
    } else {
        match (s.devices)() {
            Err(error) => Rename::No(format!(
                "the list of paired devices could not be read ({error})"
            )),
            Ok(devices) => {
                let proposed = format!("{}.{}", run.random_name()?, node.suffix);
                let free = one_label(&proposed, &node.suffix)
                    && matches!(
                        origin_plan(&start.config, &proposed, s.port),
                        Ok(OriginPlan::Free(_))
                    );
                if free {
                    Rename::Offer { proposed, devices }
                } else {
                    Rename::No(format!("{proposed} is not free"))
                }
            }
        }
    };

    // Decide the name, with consent.
    let offer = match &rename {
        Rename::Offer { proposed, devices } => Some((proposed.clone(), *devices)),
        Rename::No(_) => None,
    };
    let renaming = match flag {
        NameFlag::Random => {
            let (proposed, devices) = match (&offer, &rename) {
                (Some(offer), _) => offer.clone(),
                (None, Rename::No(why)) => {
                    return halt(format!("A new name cannot be given: {why}."))
                }
                (None, Rename::Offer { .. }) => return halt("A new name cannot be given."),
            };
            let lines = disclosure(&node.name, Some(&proposed), &node.suffix, devices, s.port);
            consent(s, &lines, yes)?;
            true
        }
        NameFlag::Keep => {
            let lines = disclosure(&node.name, None, &node.suffix, 0, s.port);
            consent(s, &lines, yes)?;
            false
        }
        NameFlag::Ask => {
            let shown = offer.as_ref().map(|(proposed, _)| proposed.as_str());
            let devices = offer.as_ref().map_or(0, |(_, devices)| *devices);
            for line in disclosure(&node.name, shown, &node.suffix, devices, s.port) {
                s.console.say(&line);
            }
            if !s.interactive {
                return halt(
                    "Without a terminal to ask in, run again with --yes and either \
                     --random-name or --keep-name.",
                );
            }
            if let Rename::No(why) = &rename {
                s.console.say(&format!("A new name is not offered: {why}."));
            }
            let choices: &[Choice] = if offer.is_some() {
                &[Choice::RandomName, Choice::KeepName, Choice::Cancel]
            } else {
                &[Choice::KeepName, Choice::Cancel]
            };
            match s.console.choose("Set up the phone address?", choices) {
                Choice::RandomName if offer.is_some() => true,
                Choice::KeepName => false,
                _ => return halt("Cancelled."),
            }
        }
    };
    let target = match (&offer, renaming) {
        (Some((proposed, _)), true) => proposed.clone(),
        _ => node.name.clone(),
    };

    // From here on, every write follows a fresh, validated reading.
    let mut probed = false;
    if renaming {
        let now = fresh(s.daemon, &node).await?;
        check_rename(s, &now, &node, &target, &start_hostname).await?;
        probe(run, &now).await?;
        probed = true;
        let now = fresh(s.daemon, &node).await?;
        check_rename(s, &now, &node, &target, &start_hostname).await?;
        rename_node(run, &node, &start_hostname, &target).await?;
    }

    let origin = format!("https://{target}");
    let mut wrote = false;
    let served = loop {
        let now = fresh(s.daemon, &node).await?;
        expect_name(&now, &target)?;
        let current = eligible(&now.status).or_else(|refusal| refused(&refusal))?;
        let plan =
            origin_plan(&now.config, &target, s.port).or_else(|refusal| refused(&refusal))?;
        match plan {
            OriginPlan::AlreadyServed => break current,
            OriginPlan::Free(_) if !probed => {
                probe(run, &now).await?;
                probed = true;
            }
            OriginPlan::Free(planned) => {
                let tag = version_tag(now.etag.as_deref()).or_else(|refusal| refused(&refusal))?;
                serve(run, &current, &planned, &tag).await?;
                wrote = true;
                break current;
            }
        }
    };
    if !wrote {
        let owned_here = run
            .journal
            .serve
            .iter()
            .any(|owned| owned.name.eq_ignore_ascii_case(&served.name) && owned.port == s.port);
        if owned_here {
            s.console
                .say(&format!("{origin}/ is set up by LocalPilot already."));
        } else {
            s.console.say(&format!(
                "{origin}/ already forwards to the gateway, but LocalPilot has no confirmed \
                 record of setting it up; it uses it and will never remove it."
            ));
        }
    }

    // The user config.
    match (s.edit_config)(s.config_path.as_deref(), &origin) {
        Ok(ConfigEdit::Written) => {
            s.console.say(&format!(
                "Your config now has [remote] enabled = true and advertised_url = \"{origin}\"."
            ));
            run.done.push("updated your config".to_string());
        }
        Ok(ConfigEdit::Unchanged) => {}
        Err(ConfigError::Before(error)) => {
            return halt(format!(
                "Your config was not updated: {error}. Set [remote] enabled = true and \
                 advertised_url = \"{origin}\" yourself."
            ))
        }
        Err(ConfigError::After(error)) => {
            return unknown(format!(
                "Setup replaced your config to set [remote] enabled = true and advertised_url \
                 = \"{origin}\", but {error}. Check the file. Nothing else was done."
            ))
        }
    }

    // The gateway, then the address.
    match s.probes.listener(s.port).await {
        Listener::NotRunning => {
            s.console.say(&format!(
                "The gateway is not running. Start it with `localpilot remote gateway`, then \
                 run `localpilot remote setup` again to check {origin} from this computer."
            ));
            return Ok(());
        }
        Listener::Unidentified(what) => {
            s.console.say(&format!(
                "Something on port {} is not the LocalPilot gateway ({what}). Check what runs \
                 there; setup does not touch it.",
                s.port
            ));
            return Ok(());
        }
        Listener::Gateway => {}
    }
    match s.probes.origin(s.port, &target).await {
        OriginAnswer::Current => {}
        OriginAnswer::OtherPolicy(why) => {
            s.console.say(&format!(
                "The gateway is running with an older address ({why}). Stop it and start it \
                 again with `localpilot remote gateway`, then run setup again to check."
            ));
            return Ok(());
        }
        OriginAnswer::Unidentified(why) => {
            s.console.say(&format!(
                "The gateway answered the address check in a way setup does not recognise \
                 ({why}); the address is not confirmed."
            ));
            return Ok(());
        }
    }
    let now = fresh(s.daemon, &node).await?;
    expect_name(&now, &target)?;
    eligible(&now.status).or_else(|refusal| refused(&refusal))?;
    if origin_plan(&now.config, &target, s.port).or_else(|refusal| refused(&refusal))?
        != OriginPlan::AlreadyServed
    {
        return halt("Tailscale's Serve settings changed before the check; it was not run.");
    }
    // The first HTTPS connection to a name Tailscale has no certificate for
    // yet waits while Tailscale gets one, which can take up to a minute; say
    // so, or the command looks stuck.
    s.console.say(&format!(
        "Checking {origin} from this computer. Right after a new name, Tailscale first gets \
         its HTTPS certificate, which can take up to a minute."
    ));
    if let Err(why) = s.probes.tls(&origin).await {
        return halt(format!(
            "The check of {origin} from this computer failed: {why}."
        ));
    }
    let mut marked = false;
    for owned in &mut run.journal.serve {
        if owned.name.eq_ignore_ascii_case(&target) && owned.port == s.port && !owned.verified {
            owned.verified = true;
            marked = true;
        }
    }
    if marked {
        if let Err(error) = run.save() {
            return halt(format!(
                "{origin} works from this computer, but LocalPilot could not record that \
                 ({error})."
            ));
        }
    }
    s.console.say(&format!(
        "Verified from this computer; the phone is not checked yet. Pair it with \
         `localpilot devices pair` and open {origin} on the phone."
    ));
    Ok(())
}

/// The rename's preconditions on a fresh reading: the same name as at the
/// start, nothing using it, the proposed name free, and the name setting
/// unchanged.
async fn check_rename(
    s: &Setup<'_>,
    now: &Reading,
    start: &Node,
    proposed: &str,
    hostname_was: &str,
) -> Step<()> {
    expect_name(now, &start.name)?;
    eligible(&now.status).or_else(|refusal| refused(&refusal))?;
    if let Some(use_) = name_in_use(&now.config, &start.name) {
        return halt(format!("{use_}, and a new name would move it."));
    }
    if !matches!(
        origin_plan(&now.config, proposed, s.port),
        Ok(OriginPlan::Free(_))
    ) {
        return halt(format!("{proposed} is no longer free."));
    }
    if hostname(s.daemon).await? != hostname_was {
        return halt("Tailscale's name setting changed while setting up.");
    }
    Ok(())
}

/// The test save: the settings exactly as just read, with a version tag that
/// is deliberately not theirs. Only a refusal passes.
async fn probe(run: &mut Run<'_, '_>, now: &Reading) -> Step<()> {
    let observed = now.etag.clone().unwrap_or_default();
    let mut tag = format!("lp-probe-{}", run.random_hex(8)?);
    while tag == observed {
        tag = format!("lp-probe-{}", run.random_hex(8)?);
    }
    let body = if now.config.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        now.config.clone()
    };
    run.plan(Attempt::new(Op::Probe, &now.node))?;
    let (state, halted) = match run.s.daemon.post(SERVE, &body, &tag).await {
        Ok(reply) if reply.status == 412 => (State::Passed, None),
        Ok(reply) if reply.status == 200 => (
            State::Unexpected,
            Some(Halt::Unknown(
                "Tailscale accepted a save it should have refused, so on this install it does \
                 not protect settings that changed in between. That save wrote back your Serve \
                 settings as they were just read; a change made elsewhere at that moment may \
                 have been undone. Check `tailscale serve status`. Nothing else was done."
                    .to_string(),
            )),
        ),
        Ok(reply) if matches!(reply.status, 401 | 403) => (
            State::Refused,
            Some(Halt::Clean(format!(
                "Tailscale refused to let this user change its settings ({}). On Linux, `sudo \
                 tailscale set --operator=$USER` lets your user manage Tailscale.",
                reply.status
            ))),
        ),
        Ok(reply) => (
            State::Unresolved,
            Some(Halt::Unknown(format!(
                "Tailscale answered the test save with {}, which setup does not expect, so \
                 whether it saved is not known. Check `tailscale serve status`. Nothing else \
                 was done.",
                reply.status
            ))),
        ),
        Err(failure) => (
            State::Unresolved,
            Some(Halt::Unknown(format!(
                "{failure} during the test save, so whether it saved is not known. Check \
                 `tailscale serve status`. Nothing else was done."
            ))),
        ),
    };
    let detail = match &halted {
        Some(Halt::Clean(message) | Halt::Unknown(message)) => message.clone(),
        None => String::new(),
    };
    let recorded = run.settle(state, &detail);
    if let Some(halted) = halted {
        return Err(halted);
    }
    recorded.map_err(|error| {
        Halt::Clean(format!(
            "LocalPilot could not record the test save ({error})."
        ))
    })
}

async fn rename_node(run: &mut Run<'_, '_>, node: &Node, old: &str, target: &str) -> Step<()> {
    let s = run.s;
    let label = target.split('.').next().unwrap_or_default().to_string();
    let mut attempt = Attempt::new(Op::Rename, node);
    attempt.old_hostname = Some(old.to_string());
    attempt.new_hostname = Some(label.clone());
    run.plan(attempt)?;
    let body = serde_json::json!({ "Hostname": label, "HostnameSet": true });
    let answered = s.daemon.patch(PREFS, &body).await;
    let confirmed = match &answered {
        Ok(reply) if reply.status == 200 && !reply.truncated => {
            let decoded = serde_json::from_slice::<Value>(&reply.body)
                .ok()
                .and_then(|prefs| {
                    prefs
                        .get("Hostname")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
            if decoded.as_deref() == Some(label.as_str()) {
                let again = hostname(s.daemon).await.ok();
                let status = read(s.daemon).await.ok();
                again.as_deref() == Some(label.as_str())
                    && status.is_some_and(|reading| {
                        same_node(&reading.node, &node.node_id, &node.tailnet)
                    })
            } else {
                false
            }
        }
        _ => false,
    };
    if !confirmed {
        let halted = match &answered {
            Ok(reply) if reply.status == 403 => {
                Halt::Clean("Tailscale refused to let this user rename the computer.".to_string())
            }
            Ok(reply) => Halt::Unknown(format!(
                "Tailscale's answer to the rename ({}) could not be confirmed, so whether this \
                 computer was renamed to {label} is not known. Check the name with `tailscale \
                 status`. Nothing else was done.",
                reply.status
            )),
            Err(failure) => Halt::Unknown(format!(
                "{failure} during the rename, so whether this computer was renamed to {label} \
                 is not known. Check the name with `tailscale status`. Nothing else was done."
            )),
        };
        let (state, detail) = match &halted {
            Halt::Clean(message) => (State::Refused, message.clone()),
            Halt::Unknown(message) => (State::Unresolved, message.clone()),
        };
        let _ = run.settle(state, &detail);
        return Err(halted);
    }
    run.journal.rename = Some(RenameOwned {
        node_id: node.node_id.clone(),
        tailnet: node.tailnet.clone(),
        old_hostname: old.to_string(),
        new_hostname: label.clone(),
    });
    run.done
        .push(format!("renamed this computer in Tailscale to {label}"));
    if let Err(error) = run.settle(State::Done, "") {
        return halt(format!(
            "LocalPilot could not record the rename ({error}); restore the old name in \
             Tailscale yourself if you want it back."
        ));
    }
    s.console
        .say(&format!("Renamed this computer in Tailscale to {label}."));

    // Wait for the new name, with its certificate, on this same node.
    let deadline = tokio::time::Instant::now() + s.settle_within;
    let mut observed = node.name.clone();
    loop {
        tokio::time::sleep(SETTLE_EVERY).await;
        if let Ok((status, _)) = document(s.daemon, STATUS, "status").await {
            if let Ok(now) = read_node(&status) {
                if !same_node(&now, &node.node_id, &node.tailnet) {
                    return halt("Tailscale is now signed in as another node or tailnet.");
                }
                observed.clone_from(&now.name);
                if now.name.eq_ignore_ascii_case(target) && eligible(&status).is_ok() {
                    return Ok(());
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return halt(format!(
                "Tailscale now reports this computer as {observed}, not yet {target} with a \
                 certificate. It may still be updating, may have added a suffix to avoid a \
                 clash, or the name may be set in the admin console. Run `localpilot remote \
                 setup` again in a minute, or check the name in the Tailscale admin console."
            ));
        }
    }
}

async fn serve(run: &mut Run<'_, '_>, node: &Node, planned: &Value, tag: &str) -> Step<()> {
    let s = run.s;
    run.plan(Attempt::new(Op::Serve, node))?;
    let answered = s.daemon.post(SERVE, planned, tag).await;
    let origin = format!("https://{}", node.name);
    match &answered {
        Ok(reply) if reply.status == 200 => {
            let confirmed = match read(s.daemon).await {
                Ok(after) => {
                    same_node(&after.node, &node.node_id, &node.tailnet)
                        && origin_plan(&after.config, &node.name, s.port)
                            == Ok(OriginPlan::AlreadyServed)
                }
                Err(_) => false,
            };
            if !confirmed {
                let message = format!(
                    "Tailscale accepted the change, but a fresh read did not show {origin} set \
                     up as planned, so LocalPilot does not claim it. Check `tailscale serve \
                     status`. Nothing else was done."
                );
                let _ = run.settle(State::Unresolved, &message);
                return unknown(message);
            }
            let record = ServeOwned {
                node_id: node.node_id.clone(),
                tailnet: node.tailnet.clone(),
                name: node.name.clone(),
                port: s.port,
                verified: false,
            };
            let known =
                run.journal.serve.iter().any(|owned| {
                    owned.name.eq_ignore_ascii_case(&record.name) && owned.port == s.port
                });
            if !known {
                run.journal.serve.push(record);
            }
            run.done.push(format!("Tailscale now serves {origin}/"));
            if let Err(error) = run.settle(State::Done, "") {
                return halt(format!("LocalPilot could not record that ({error})."));
            }
            s.console
                .say(&format!("Tailscale now serves {origin}/ from the gateway."));
            Ok(())
        }
        Ok(reply) => {
            let halted = match reply.status {
                412 => Halt::Clean(
                    "Tailscale's Serve settings changed while setting up. Run setup again."
                        .to_string(),
                ),
                401 | 403 => Halt::Clean(format!(
                    "Tailscale refused the change ({}).{}",
                    reply.status,
                    crate::remote_tailscale::admin_note(planned)
                        .map(|note| format!(" Note: {note}."))
                        .unwrap_or_default()
                )),
                status => Halt::Unknown(format!(
                    "Tailscale answered {status}, so whether {origin} was set up is not known. \
                     Check `tailscale serve status`. Nothing else was done."
                )),
            };
            let (state, detail) = match &halted {
                Halt::Clean(message) => (State::Refused, message.clone()),
                Halt::Unknown(message) => (State::Unresolved, message.clone()),
            };
            let _ = run.settle(state, &detail);
            Err(halted)
        }
        Err(failure) => {
            let message = format!(
                "{failure} while setting up {origin}, so whether it was set up is not known. \
                 Check `tailscale serve status`. Nothing else was done."
            );
            let _ = run.settle(State::Unresolved, &message);
            unknown(message)
        }
    }
}

// ------------------------------------------------------------- config

/// What happened to the user config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConfigEdit {
    /// It already said this; nothing was written.
    Unchanged,
    /// It was replaced and reads back exactly as written.
    Written,
}

/// Why the config edit did not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConfigError {
    /// Nothing was replaced.
    Before(String),
    /// The file was replaced, but reading it back failed or showed something
    /// else; its current content is not confirmed.
    After(String),
}

/// Set `[remote] enabled = true` and `advertised_url` in the user config,
/// keeping everything else byte for byte. Every LocalPilot writer of this
/// file takes the same exclusive lock; an editor outside LocalPilot that
/// writes between the last check and the replace is not detected.
pub(crate) fn edit_config(path: Option<&Path>, origin: &str) -> Result<ConfigEdit, ConfigError> {
    edit_config_with(path, origin, &|_| {})
}

/// [`edit_config`], with a hook that runs right after the replace (tests use
/// it to stand in for another writer).
pub(crate) fn edit_config_with(
    path: Option<&Path>,
    origin: &str,
    after_replace: &dyn Fn(&Path),
) -> Result<ConfigEdit, ConfigError> {
    let before_ = |message: String| ConfigError::Before(message);
    let path =
        path.ok_or_else(|| before_("there is no user config location on this system".into()))?;
    let directory = path
        .parent()
        .ok_or_else(|| before_("the config path has no folder".into()))?;
    std::fs::create_dir_all(directory).map_err(|error| before_(error.to_string()))?;
    let lock_path = path.with_extension("toml.lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|error| before_(format!("could not open {}: {error}", lock_path.display())))?;
    let mut lock = fd_lock::RwLock::new(lock_file);
    let _guard = lock
        .try_write()
        .map_err(|_| before_("another LocalPilot command is writing the config".into()))?;

    let before = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(before_(format!(
                "could not read {}: {error}",
                path.display()
            )))
        }
    };
    let text =
        String::from_utf8(before.clone()).map_err(|_| before_("the config is not UTF-8".into()))?;
    let mut document: toml_edit::DocumentMut = text
        .parse()
        .map_err(|error| before_(format!("the config is not valid TOML ({error})")))?;
    let remote = document
        .entry("remote")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
    let table = remote
        .as_table_mut()
        .ok_or_else(|| before_("[remote] in the config is not a table".into()))?;
    table["enabled"] = toml_edit::value(true);
    table["advertised_url"] = toml_edit::value(origin);
    let after = document.to_string();
    if after.as_bytes() == before.as_slice() {
        return Ok(ConfigEdit::Unchanged);
    }
    // Replace only what was read.
    let now = std::fs::read(path).unwrap_or_default();
    if now != before {
        return Err(before_(
            "the config changed while setup was editing it".into(),
        ));
    }
    let temporary = path.with_extension("toml.tmp");
    std::fs::write(&temporary, after.as_bytes()).map_err(|error| before_(error.to_string()))?;
    std::fs::rename(&temporary, path).map_err(|error| before_(error.to_string()))?;
    after_replace(path);
    let written = std::fs::read(path)
        .map_err(|error| ConfigError::After(format!("reading it back failed ({error})")))?;
    if written != after.as_bytes() {
        return Err(ConfigError::After(
            "it reads back differently, so something else changed it right after".into(),
        ));
    }
    Ok(ConfigEdit::Written)
}

// ------------------------------------------------------------- undo

/// `localpilot remote setup --undo`: remove what LocalPilot confirmed it set
/// up, each part only while it is still exactly as recorded.
pub(crate) async fn undo(s: &Setup<'_>, yes: bool) -> Result<(), Stop> {
    with_journal(s, |run| Box::pin(undo_inner(run, yes))).await
}

async fn undo_inner(run: &mut Run<'_, '_>, yes: bool) -> Step<()> {
    let s = run.s;
    no_pending(&run.journal)?;
    if run.journal.serve.is_empty() && run.journal.rename.is_none() {
        s.console
            .say("LocalPilot has not set anything up in Tailscale on this computer.");
        return Ok(());
    }
    let start = read(s.daemon).await?;
    for (node_id, tailnet) in owners(&run.journal) {
        if !same_node(&start.node, &node_id, &tailnet) {
            return halt(
                "Tailscale is signed in as another node or tailnet than the one LocalPilot set up.",
            );
        }
    }
    let node = start.node.clone();
    let mut lines = Vec::new();
    for owned in &run.journal.serve {
        lines.push(format!(
            "Undo removes https://{}/ from Tailscale Serve.",
            owned.name
        ));
    }
    if let Some(owned) = &run.journal.rename {
        let old = if owned.old_hostname.is_empty() {
            "this computer's own host name".to_string()
        } else {
            owned.old_hostname.clone()
        };
        lines.push(format!(
            "Undo restores Tailscale's name setting for this computer to {old}. Tailscale may \
             not give back the exact earlier name, and a change made to the name elsewhere at \
             the same moment could be overwritten."
        ));
    }
    lines.push(PROBE_LIMIT.to_string());
    for line in &lines {
        s.console.say(line);
    }
    if !yes {
        if !s.interactive {
            return halt("Without a terminal to ask in, add --yes to confirm.");
        }
        if s.console
            .choose("Undo?", &[Choice::Proceed, Choice::Cancel])
            != Choice::Proceed
        {
            return halt("Cancelled.");
        }
    }
    let now = fresh(s.daemon, &node).await?;
    probe(run, &now).await?;

    for owned in run.journal.serve.clone() {
        let now = fresh(s.daemon, &node).await?;
        match origin_plan(&now.config, &owned.name, owned.port) {
            Ok(OriginPlan::Free(_)) => {
                s.console
                    .say(&format!("https://{}/ was already removed.", owned.name));
                run.journal.serve.retain(|item| item != &owned);
                run.save()
                    .map_err(|error| Halt::Clean(format!("Could not record that ({error}).")))?;
            }
            Ok(OriginPlan::AlreadyServed) => {
                let tag = version_tag(now.etag.as_deref()).or_else(|refusal| refused(&refusal))?;
                let without = without_ours(&now.config, &owned.name);
                run.plan(Attempt::new(Op::UndoServe, &node))?;
                let answered = s.daemon.post(SERVE, &without, &tag).await;
                let gone = match &answered {
                    Ok(reply) if reply.status == 200 => match read(s.daemon).await {
                        Ok(after) => {
                            same_node(&after.node, &owned.node_id, &owned.tailnet)
                                && matches!(
                                    origin_plan(&after.config, &owned.name, owned.port),
                                    Ok(OriginPlan::Free(_))
                                )
                        }
                        Err(_) => false,
                    },
                    _ => false,
                };
                if !gone {
                    let halted = match &answered {
                        Ok(reply) if matches!(reply.status, 401 | 403 | 412) => {
                            Halt::Clean(format!(
                                "Tailscale did not remove https://{}/ ({}). Run undo again.",
                                owned.name, reply.status
                            ))
                        }
                        _ => Halt::Unknown(format!(
                            "Whether https://{}/ was removed is not known; LocalPilot keeps its \
                             record. Check `tailscale serve status`.",
                            owned.name
                        )),
                    };
                    let (state, detail) = match &halted {
                        Halt::Clean(message) => (State::Refused, message.clone()),
                        Halt::Unknown(message) => (State::Unresolved, message.clone()),
                    };
                    let _ = run.settle(state, &detail);
                    return Err(halted);
                }
                run.journal.serve.retain(|item| item != &owned);
                run.done.push(format!(
                    "removed https://{}/ from Tailscale Serve",
                    owned.name
                ));
                if let Err(error) = run.settle(State::Done, "") {
                    return halt(format!(
                        "LocalPilot could not record the removal ({error})."
                    ));
                }
                s.console.say(&format!(
                    "Removed https://{}/ from Tailscale Serve.",
                    owned.name
                ));
            }
            Err(refusal) => {
                return halt(format!(
                    "https://{}/ is no longer exactly as LocalPilot set it up ({}); it was not \
                     removed.",
                    owned.name,
                    refusal.message()
                ))
            }
        }
    }

    if let Some(owned) = run.journal.rename.clone() {
        let now = fresh(s.daemon, &node).await?;
        if let Some(use_) = name_in_use(&now.config, &now.node.name) {
            return halt(format!(
                "{use_}, and restoring the old name would move it; the name was not changed."
            ));
        }
        if hostname(s.daemon).await? != owned.new_hostname {
            return halt(
                "Tailscale's name setting was changed after setup; LocalPilot leaves it as it is.",
            );
        }
        let mut attempt = Attempt::new(Op::UndoRename, &node);
        attempt.old_hostname = Some(owned.new_hostname.clone());
        attempt.new_hostname = Some(owned.old_hostname.clone());
        run.plan(attempt)?;
        let body = serde_json::json!({ "Hostname": owned.old_hostname, "HostnameSet": true });
        let answered = s.daemon.patch(PREFS, &body).await;
        let restored = match &answered {
            Ok(reply) if reply.status == 200 => {
                hostname(s.daemon).await.ok().as_deref() == Some(owned.old_hostname.as_str())
                    && read(s.daemon)
                        .await
                        .is_ok_and(|after| same_node(&after.node, &owned.node_id, &owned.tailnet))
            }
            _ => false,
        };
        if !restored {
            let halted = match &answered {
                Ok(reply) if reply.status == 403 => {
                    Halt::Clean("Tailscale refused to restore the name.".to_string())
                }
                _ => Halt::Unknown(
                    "Whether the old name was restored is not known; LocalPilot keeps its \
                     record. Check the name with `tailscale status`."
                        .to_string(),
                ),
            };
            let (state, detail) = match &halted {
                Halt::Clean(message) => (State::Refused, message.clone()),
                Halt::Unknown(message) => (State::Unresolved, message.clone()),
            };
            let _ = run.settle(state, &detail);
            return Err(halted);
        }
        run.journal.rename = None;
        run.done
            .push("restored Tailscale's earlier name setting".to_string());
        if let Err(error) = run.settle(State::Done, "") {
            return halt(format!(
                "LocalPilot could not record the restore ({error})."
            ));
        }
        s.console
            .say("Restored Tailscale's earlier name setting for this computer.");
    }
    s.console.say(
        "Your config's [remote] advertised_url still names the old address; change or remove \
         it yourself.",
    );
    Ok(())
}

// ------------------------------------------------------------- resolve

/// `localpilot remote setup --resolve`: after the user has checked
/// Tailscale, forget operations whose outcome was never confirmed. Nothing
/// is adopted as LocalPilot's; confirmed records stay.
pub(crate) async fn resolve(s: &Setup<'_>, yes: bool) -> Result<(), Stop> {
    with_journal(s, |run| Box::pin(resolve_inner(run, yes))).await
}

async fn resolve_inner(run: &mut Run<'_, '_>, yes: bool) -> Step<()> {
    let s = run.s;
    if run.journal.pending.is_empty() {
        s.console.say("There is nothing unconfirmed to resolve.");
        return Ok(());
    }
    for attempt in &run.journal.pending {
        s.console
            .say(&format!("Unconfirmed: {}.", attempt.describe()));
    }
    s.console.say(
        "Resolving forgets these after you have checked Tailscale yourself. LocalPilot will not \
         treat any of them as its own, so undo will not touch what they may have changed.",
    );
    if !yes {
        if !s.interactive {
            return halt("Without a terminal to ask in, add --yes to confirm.");
        }
        if s.console
            .choose("Forget them?", &[Choice::Proceed, Choice::Cancel])
            != Choice::Proceed
        {
            return halt("Cancelled.");
        }
    }
    run.journal.pending.clear();
    run.save()
        .map_err(|error| Halt::Clean(format!("Could not record that ({error}).")))?;
    s.console.say("Forgotten.");
    Ok(())
}

// ------------------------------------------------------------- production

struct Terminal;

impl Console for Terminal {
    fn say(&self, line: &str) {
        println!("{line}");
    }

    fn choose(&self, prompt: &str, choices: &[Choice]) -> Choice {
        use std::io::Write as _;
        println!("{prompt}");
        for (number, choice) in choices.iter().enumerate() {
            println!("  {}. {}", number + 1, choice.label());
        }
        print!("Choose a number (Enter cancels): ");
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).is_err() {
            return Choice::Cancel;
        }
        answer
            .trim()
            .parse::<usize>()
            .ok()
            .and_then(|number| number.checked_sub(1))
            .and_then(|index| choices.get(index).copied())
            .unwrap_or(Choice::Cancel)
    }
}

struct LocalProbes;

#[async_trait::async_trait]
impl Probes for LocalProbes {
    async fn listener(&self, port: u16) -> Listener {
        crate::remote_gateway::listener_on(port).await
    }
    async fn origin(&self, port: u16, host: &str) -> OriginAnswer {
        crate::remote_gateway::origin_on(port, host).await
    }
    async fn tls(&self, origin: &str) -> Result<(), String> {
        tls_check(origin).await
    }
}

/// The address over real TLS from this computer: the gateway's health
/// answer, then a browser-shaped upgrade that must get the exact
/// missing-credential refusal (a pre-upgrade check, not a working socket).
async fn tls_check(origin: &str) -> Result<(), String> {
    const CAP: usize = 64 * 1024;
    async fn body(mut response: reqwest::Response, cap: usize) -> Result<Vec<u8>, String> {
        let mut collected = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
            collected.extend_from_slice(&chunk);
            if collected.len() > cap {
                return Err("the answer was too long".to_string());
            }
        }
        Ok(collected)
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .http1_only()
        .timeout(TLS_CHECK_WITHIN)
        .build()
        .map_err(|error| error.to_string())?;
    let health = client
        .get(format!("{origin}/healthz"))
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = health.status().as_u16();
    let text = body(health, CAP).await?;
    if status != 200 || text != crate::remote_status::health_body().as_bytes() {
        return Err(format!(
            "the address did not reach the gateway (it answered {status})"
        ));
    }
    let upgrade = client
        .get(format!("{origin}/api/socket"))
        .header("Origin", origin)
        .header("Upgrade", "websocket")
        .header("Connection", "Upgrade")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = upgrade.status().as_u16();
    let text = body(upgrade, CAP).await?;
    if status == 401 && text == b"this device is not paired" {
        Ok(())
    } else {
        Err(format!(
            "the upgrade request through the address was answered {status}, not the gateway's \
             missing-credential refusal"
        ))
    }
}

fn paired_devices() -> Result<usize, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let registry = crate::server_devices::DeviceRegistry::for_user(&cwd);
    let devices = registry.list().map_err(|error| error.to_string())?;
    Ok(devices
        .iter()
        .filter(|device| device.revoked_unix.is_none())
        .count())
}

fn system_random(buffer: &mut [u8]) -> Result<(), String> {
    use ring::rand::SecureRandom as _;
    ring::rand::SystemRandom::new()
        .fill(buffer)
        .map_err(|_| "no random bytes available".to_string())
}

/// `~/.localpilot/remote`, beside the device registry.
fn state_dir() -> Result<PathBuf, Stop> {
    crate::server_devices::user_devices_dir()
        .and_then(|devices| devices.parent().map(|home| home.join("remote")))
        .ok_or_else(|| Stop("No home folder to keep setup's record in.".to_string()))
}

/// What `localpilot remote setup` was asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Check,
    Setup(NameFlag),
    Undo,
    Resolve,
}

/// `localpilot remote setup [...]` from the command line.
pub(crate) async fn command(mode: Mode, yes: bool) -> anyhow::Result<()> {
    use std::io::IsTerminal as _;
    let cwd = std::env::current_dir()?;
    let port = localpilot_config::load(
        &localpilot_config::ConfigPaths::standard(&cwd),
        &localpilot_config::CliOverrides::default(),
    )?
    .remote
    .port;
    let console = Terminal;
    if mode == Mode::Check {
        return check(&LocalDaemon, &console, port)
            .await
            .map_err(|error| anyhow::anyhow!("{error}"));
    }
    let devices = paired_devices;
    let setup = Setup {
        daemon: &LocalDaemon,
        probes: &LocalProbes,
        console: &console,
        devices: &devices,
        state_dir: state_dir().map_err(|error| anyhow::anyhow!("{error}"))?,
        config_path: localpilot_config::user_config_path(),
        port,
        interactive: std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        settle_within: SETTLE_WITHIN,
        persist: &write_journal,
        edit_config: &edit_config,
        random: &system_random,
    };
    let result = match mode {
        Mode::Check => Ok(()),
        Mode::Setup(flag) => run(&setup, flag, yes).await,
        Mode::Undo => undo(&setup, yes).await,
        Mode::Resolve => resolve(&setup, yes).await,
    };
    result.map_err(|error| anyhow::anyhow!("{error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    /// Another writer racing setup.
    type Race = Box<dyn FnOnce(&mut Sim) + Send>;

    // A simulated Tailscale daemon shaped from the published local API of the
    // supported releases (source-derived, not captured from a real install).
    // Writes behave like the daemon's: a POST applies only when its If-Match
    // is the current tag; a PATCH of the name setting reaches the status a
    // few reads later.
    struct Sim {
        node_id: String,
        tailnet: String,
        suffix: String,
        hostname: String,
        name: String,
        certs: Vec<String>,
        config: Value,
        generation: u64,
        pending: Option<(String, u32)>,
        cert_lag: bool,
        /// The answer to the next POST, instead of the daemon's own.
        post_answers: std::collections::VecDeque<Result<u16, ApiFailure>>,
        /// The answer to the next PATCH, instead of the daemon's own.
        patch_answer: Option<Result<u16, ApiFailure>>,
        /// Run just before the daemon handles the next POST that is not the
        /// test save (another writer racing setup).
        race: Option<Race>,
        /// Run just before the daemon handles the test save.
        on_probe: Option<Race>,
        requests: Vec<String>,
        /// The body of every POST, in order.
        bodies: Vec<Value>,
        version: String,
    }

    impl Sim {
        fn new() -> Self {
            Self {
                node_id: "nNODE1CNTRL".into(),
                tailnet: "user@example.com".into(),
                suffix: "tail1234.ts.net".into(),
                hostname: String::new(),
                name: "davids-desktop".into(),
                certs: vec!["davids-desktop".into()],
                config: Value::Null,
                generation: 1,
                pending: None,
                cert_lag: false,
                post_answers: Default::default(),
                patch_answer: None,
                race: None,
                on_probe: None,
                requests: Vec::new(),
                bodies: Vec::new(),
                version: "1.104.5-t73a48057a".into(),
            }
        }

        fn full(&self, label: &str) -> String {
            format!("{label}.{}", self.suffix)
        }

        fn status(&mut self) -> Value {
            if let Some((label, left)) = self.pending.take() {
                if left == 0 {
                    self.name.clone_from(&label);
                    if !self.cert_lag {
                        self.certs.push(label);
                    }
                } else {
                    self.pending = Some((label, left - 1));
                }
            }
            json!({
                "Version": self.version,
                "BackendState": "Running",
                "Self": {
                    "ID": self.node_id,
                    "DNSName": format!("{}.", self.full(&self.name)),
                    "CapMap": { "https": null }
                },
                "CurrentTailnet": {
                    "Name": self.tailnet,
                    "MagicDNSSuffix": self.suffix,
                    "MagicDNSEnabled": true
                },
                "CertDomains": self.certs.iter().map(|label| self.full(label)).collect::<Vec<_>>()
            })
        }

        fn tag(&self) -> String {
            format!("e{}", self.generation)
        }

        fn writes(&self) -> Vec<String> {
            self.requests
                .iter()
                .filter(|request| !request.starts_with("GET"))
                .cloned()
                .collect()
        }
    }

    fn reply(status: u16, body: Vec<u8>, etag: Option<String>) -> Reply {
        Reply {
            status,
            etag,
            body,
            truncated: false,
        }
    }

    struct Fake(Mutex<Sim>);

    impl Fake {
        fn sim(&self) -> std::sync::MutexGuard<'_, Sim> {
            self.0.lock().unwrap()
        }
    }

    #[async_trait::async_trait]
    impl Daemon for Fake {
        async fn get(&self, path: &str) -> Result<Reply, ApiFailure> {
            let mut sim = self.sim();
            sim.requests.push(format!("GET {path}"));
            Ok(match path {
                STATUS => {
                    let status = sim.status();
                    reply(200, serde_json::to_vec(&status).unwrap(), None)
                }
                SERVE => reply(
                    200,
                    serde_json::to_vec(&sim.config).unwrap(),
                    Some(sim.tag()),
                ),
                PREFS => reply(
                    200,
                    serde_json::to_vec(&json!({ "Hostname": sim.hostname, "WantRunning": true }))
                        .unwrap(),
                    None,
                ),
                _ => reply(404, Vec::new(), None),
            })
        }

        async fn post(
            &self,
            path: &str,
            body: &Value,
            if_match: &str,
        ) -> Result<Reply, ApiFailure> {
            let mut sim = self.sim();
            sim.requests.push(format!("POST {path} {if_match}"));
            sim.bodies.push(body.clone());
            if if_match.starts_with("lp-probe-") {
                if let Some(hook) = sim.on_probe.take() {
                    hook(&mut sim);
                }
            } else if let Some(race) = sim.race.take() {
                race(&mut sim);
            }
            if let Some(answer) = sim.post_answers.pop_front() {
                return answer.map(|status| reply(status, Vec::new(), None));
            }
            if if_match != sim.tag() {
                return Ok(reply(412, b"etag mismatch".to_vec(), None));
            }
            sim.config = body.clone();
            sim.generation += 1;
            Ok(reply(200, Vec::new(), None))
        }

        async fn patch(&self, path: &str, body: &Value) -> Result<Reply, ApiFailure> {
            let mut sim = self.sim();
            sim.requests.push(format!("PATCH {path}"));
            if let Some(answer) = sim.patch_answer.take() {
                return answer.map(|status| reply(status, Vec::new(), None));
            }
            let label = body["Hostname"].as_str().unwrap().to_string();
            sim.hostname.clone_from(&label);
            let effective = if label.is_empty() {
                "davids-desktop".to_string()
            } else {
                label.clone()
            };
            sim.pending = Some((effective, 2));
            Ok(reply(
                200,
                serde_json::to_vec(&json!({ "Hostname": label })).unwrap(),
                None,
            ))
        }
    }

    struct FakeProbes {
        listener: Listener,
        origin: OriginAnswer,
        tls: Result<(), String>,
        tls_calls: Mutex<usize>,
    }

    impl Default for FakeProbes {
        fn default() -> Self {
            Self {
                listener: Listener::Gateway,
                origin: OriginAnswer::Current,
                tls: Ok(()),
                tls_calls: Mutex::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl Probes for FakeProbes {
        async fn listener(&self, _port: u16) -> Listener {
            self.listener.clone()
        }
        async fn origin(&self, _port: u16, _host: &str) -> OriginAnswer {
            self.origin.clone()
        }
        async fn tls(&self, _origin: &str) -> Result<(), String> {
            *self.tls_calls.lock().unwrap() += 1;
            self.tls.clone()
        }
    }

    struct FakeConsole {
        lines: Mutex<Vec<String>>,
        answers: Mutex<Vec<Choice>>,
    }

    impl FakeConsole {
        fn answering(answers: &[Choice]) -> Self {
            Self {
                lines: Mutex::new(Vec::new()),
                answers: Mutex::new(answers.iter().rev().copied().collect()),
            }
        }
        fn text(&self) -> String {
            self.lines.lock().unwrap().join("\n")
        }
    }

    impl Console for FakeConsole {
        fn say(&self, line: &str) {
            self.lines.lock().unwrap().push(line.to_string());
        }
        fn choose(&self, prompt: &str, choices: &[Choice]) -> Choice {
            self.say(&format!("? {prompt} {choices:?}"));
            let answer = self.answers.lock().unwrap().pop().unwrap_or(Choice::Cancel);
            if choices.contains(&answer) {
                answer
            } else {
                Choice::Cancel
            }
        }
    }

    const PORT: u16 = 47610;

    struct World {
        daemon: Fake,
        probes: FakeProbes,
        console: FakeConsole,
        devices: Result<usize, String>,
        home: tempfile::TempDir,
        interactive: bool,
        /// Fail to record the journal once it reaches this step and state.
        fail_persist: Option<(Op, State)>,
        random_fails: bool,
        edit_config: Box<EditConfig>,
    }

    impl World {
        fn new() -> Self {
            Self {
                daemon: Fake(Mutex::new(Sim::new())),
                probes: FakeProbes::default(),
                console: FakeConsole::answering(&[]),
                devices: Ok(0),
                home: tempfile::tempdir().unwrap(),
                interactive: false,
                fail_persist: None,
                random_fails: false,
                edit_config: Box::new(edit_config),
            }
        }

        fn config_path(&self) -> PathBuf {
            self.home.path().join("config").join("config.toml")
        }

        fn state_dir(&self) -> PathBuf {
            self.home.path().join("remote")
        }

        fn journal(&self) -> Journal {
            load_journal(&self.state_dir()).unwrap()
        }

        async fn go(&self, flag: NameFlag, yes: bool) -> Result<(), Stop> {
            self.with(|setup| Box::pin(run(setup, flag, yes))).await
        }

        async fn undo(&self, yes: bool) -> Result<(), Stop> {
            self.with(|setup| Box::pin(undo(setup, yes))).await
        }

        async fn resolve(&self, yes: bool) -> Result<(), Stop> {
            self.with(|setup| Box::pin(resolve(setup, yes))).await
        }

        async fn with<F>(&self, f: F) -> Result<(), Stop>
        where
            F: for<'s> FnOnce(
                &'s Setup<'s>,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<(), Stop>> + 's>,
            >,
        {
            let devices = self.devices.clone();
            let count = move || devices.clone();
            let fail = self.fail_persist;
            let persist = move |dir: &Path, journal: &Journal| {
                let failing = fail.is_some_and(|(step, state)| {
                    journal
                        .last
                        .as_ref()
                        .is_some_and(|last| last.step == step && last.state == state)
                });
                if failing {
                    return Err(std::io::Error::other("disk full"));
                }
                write_journal(dir, journal)
            };
            let counter = std::sync::atomic::AtomicU8::new(0);
            let fails = self.random_fails;
            let random = move |buffer: &mut [u8]| {
                if fails {
                    return Err("no entropy".to_string());
                }
                let base = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                for (index, byte) in buffer.iter_mut().enumerate() {
                    *byte = base.wrapping_mul(31).wrapping_add(index as u8 * 7);
                }
                Ok(())
            };
            let setup = Setup {
                daemon: &self.daemon,
                probes: &self.probes,
                console: &self.console,
                devices: &count,
                state_dir: self.state_dir(),
                config_path: Some(self.config_path()),
                port: PORT,
                interactive: self.interactive,
                settle_within: SETTLE_WITHIN,
                persist: &persist,
                edit_config: &self.edit_config,
                random: &random,
            };
            f(&setup).await
        }
    }

    fn ours(name: &str) -> Value {
        json!({
            "TCP": { "443": { "HTTPS": true } },
            "Web": { format!("{name}:443"): { "Handlers": { "/": { "Proxy": "http://127.0.0.1:47610" } } } }
        })
    }

    #[tokio::test(start_paused = true)]
    async fn keeping_the_name_sets_up_and_records_only_what_it_confirmed() {
        let world = World::new();
        world.go(NameFlag::Keep, true).await.expect("setup");
        let (writes, config) = {
            let sim = world.daemon.sim();
            (sim.writes(), sim.config.clone())
        };
        // The test save went first, with a tag that was not the daemon's.
        assert_eq!(writes.len(), 2, "{writes:?}");
        assert!(writes[0].starts_with("POST /localapi/v0/serve-config lp-probe-"));
        assert_eq!(writes[1], "POST /localapi/v0/serve-config e1");
        assert_eq!(config, ours("davids-desktop.tail1234.ts.net"));
        let journal = world.journal();
        assert_eq!(
            journal.serve,
            vec![ServeOwned {
                node_id: "nNODE1CNTRL".into(),
                tailnet: "user@example.com".into(),
                name: "davids-desktop.tail1234.ts.net".into(),
                port: PORT,
                verified: true,
            }]
        );
        assert_eq!(journal.rename, None);
        let config = std::fs::read_to_string(world.config_path()).unwrap();
        assert!(config.contains("advertised_url = \"https://davids-desktop.tail1234.ts.net\""));
        let said = world.console.text();
        let checking = said
            .find("Checking https://davids-desktop.tail1234.ts.net from this computer")
            .expect("the address check is announced before it runs");
        let verified = said
            .find("Verified from this computer; the phone is not checked yet")
            .expect("verified");
        assert!(checking < verified, "{said}");

        // A second run changes nothing and keeps the record.
        world.go(NameFlag::Keep, true).await.expect("again");
        assert_eq!(world.daemon.sim().writes().len(), 2);
        assert_eq!(world.journal().serve, journal.serve);
    }

    #[tokio::test(start_paused = true)]
    async fn a_random_name_settles_with_its_certificate_before_anything_is_served() {
        let world = World::new();
        world.go(NameFlag::Random, true).await.expect("setup");
        let sim = world.daemon.sim();
        let writes = sim.writes();
        assert!(
            writes[0].starts_with("POST /localapi/v0/serve-config lp-probe-"),
            "{writes:?}"
        );
        assert_eq!(writes[1], "PATCH /localapi/v0/prefs");
        assert!(
            writes[2].starts_with("POST /localapi/v0/serve-config e"),
            "{writes:?}"
        );
        let new_name = sim.name.clone();
        assert!(
            new_name.starts_with("desktop-")
                && new_name.len() == 15
                && new_name[8..]
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
            "{new_name}"
        );
        assert_eq!(sim.config, ours(&sim.full(&new_name)));
        drop(sim);
        let journal = world.journal();
        let rename = journal.rename.expect("the rename is recorded");
        assert_eq!(
            rename.old_hostname, "",
            "the OS host name fallback is kept exactly"
        );
        assert_eq!(rename.new_hostname, new_name);
        assert!(world.console.text().contains("permanently in public"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_lagging_certificate_stops_before_serve_and_tls() {
        let world = World::new();
        world.daemon.sim().cert_lag = true;
        let error = world.go(NameFlag::Random, true).await.unwrap_err();
        assert!(error.0.contains("not yet"), "{error}");
        let writes = world.daemon.sim().writes();
        assert_eq!(writes.len(), 2, "test save and rename only: {writes:?}");
        assert_eq!(*world.probes.tls_calls.lock().unwrap(), 0);
        // The rename is ours and survives the stop; nothing is served.
        assert!(world.journal().rename.is_some());
        assert!(world.journal().serve.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn an_unreadable_device_list_refuses_a_rename_before_any_write() {
        let mut world = World::new();
        world.devices = Err("corrupt".into());
        let error = world.go(NameFlag::Random, true).await.unwrap_err();
        assert!(
            error.0.contains("paired devices could not be read"),
            "{error}"
        );
        assert!(world.daemon.sim().writes().is_empty());
        // Known paired devices are allowed with the explicit flag, with the
        // same disclosure.
        let mut world = World::new();
        world.devices = Ok(2);
        world.go(NameFlag::Random, true).await.expect("setup");
        assert!(world
            .console
            .text()
            .contains("Your 2 paired device(s) will have to pair again."));
    }

    #[tokio::test(start_paused = true)]
    async fn any_use_of_the_current_name_blocks_a_rename_before_any_write() {
        for config in [
            json!({ "Web": { "davids-desktop.tail1234.ts.net:8443": { "Handlers": { "/": { "Proxy": "http://127.0.0.1:3000" } } } },
                    "TCP": { "8443": { "HTTPS": true } } }),
            json!({ "TCP": { "22": { "TCPForward": "127.0.0.1:22" } } }),
        ] {
            let world = World::new();
            world.daemon.sim().config = config.clone();
            let error = world.go(NameFlag::Random, true).await.unwrap_err();
            assert!(error.0.contains("would move it"), "{error}");
            assert!(world.daemon.sim().writes().is_empty(), "{config}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_test_save_must_be_refused() {
        for (answer, word) in [
            (Ok(200), "may have been undone"),
            (Ok(403), "refused to let this user"),
            (Ok(500), "does not expect"),
            (
                Err(ApiFailure::Interrupted("timed out".into())),
                "during the test save",
            ),
        ] {
            let world = World::new();
            world.daemon.sim().post_answers.push_back(answer.clone());
            let error = world.go(NameFlag::Keep, true).await.unwrap_err();
            assert!(error.0.contains(word), "{answer:?}: {error}");
            assert_eq!(world.daemon.sim().writes().len(), 1, "{answer:?}");
            let journal = world.journal();
            assert!(journal.serve.is_empty());
            assert_eq!(journal.last.unwrap().step, Op::Probe);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_concurrent_edit_makes_the_write_refused_and_nothing_is_claimed() {
        let world = World::new();
        world.daemon.sim().race = Some(Box::new(|sim: &mut Sim| {
            sim.config = json!({ "TCP": { "8080": { "HTTP": true } } });
            sim.generation += 1;
        }));
        let error = world.go(NameFlag::Keep, true).await.unwrap_err();
        assert!(error.0.contains("changed while setting up"), "{error}");
        assert_eq!(
            world.daemon.sim().config,
            json!({ "TCP": { "8080": { "HTTP": true } } })
        );
        let journal = world.journal();
        assert!(journal.serve.is_empty());
        assert_eq!(journal.last.unwrap().state, State::Refused);
        assert!(!world.config_path().exists(), "the config is not touched");
    }

    #[tokio::test(start_paused = true)]
    async fn an_unknown_write_outcome_stays_unresolved_and_goes_no_further() {
        for answer in [Ok(500), Err(ApiFailure::Interrupted("timed out".into()))] {
            let world = World::new();
            {
                let mut sim = world.daemon.sim();
                sim.post_answers.push_back(Ok(412));
                sim.post_answers.push_back(answer.clone());
            }
            let error = world.go(NameFlag::Keep, true).await.unwrap_err();
            assert!(error.0.contains("not known"), "{error}");
            let journal = world.journal();
            assert!(journal.serve.is_empty(), "nothing is claimed");
            assert_eq!(journal.last.unwrap().state, State::Unresolved);
            assert!(!world.config_path().exists());
            assert_eq!(*world.probes.tls_calls.lock().unwrap(), 0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_accepted_write_the_fresh_read_does_not_show_is_not_claimed() {
        let world = World::new();
        {
            let mut sim = world.daemon.sim();
            sim.post_answers.push_back(Ok(412));
            // The daemon says yes but stores nothing.
            sim.post_answers.push_back(Ok(200));
        }
        let error = world.go(NameFlag::Keep, true).await.unwrap_err();
        assert!(error.0.contains("does not claim it"), "{error}");
        assert!(world.journal().serve.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn failing_to_record_an_applied_change_stops_and_says_so() {
        let mut world = World::new();
        world.fail_persist = Some((Op::Serve, State::Done));
        let error = world.go(NameFlag::Keep, true).await.unwrap_err();
        assert!(error.0.contains("could not record"), "{error}");
        assert!(!world.config_path().exists(), "nothing else was done");
    }

    #[tokio::test(start_paused = true)]
    async fn an_unconfirmed_rename_is_never_owned() {
        for answer in [
            Ok(400),
            Ok(500),
            Err(ApiFailure::Interrupted("timed out".into())),
        ] {
            let world = World::new();
            world.daemon.sim().patch_answer = Some(answer.clone());
            let error = world.go(NameFlag::Random, true).await.unwrap_err();
            assert!(error.0.contains("is not known"), "{answer:?}: {error}");
            let journal = world.journal();
            assert!(journal.rename.is_none());
            let last = journal.last.unwrap();
            assert_eq!((last.step, last.state), (Op::Rename, State::Unresolved));
            assert_eq!(last.old_hostname.as_deref(), Some(""));
            // Nothing is served after it.
            assert_eq!(world.daemon.sim().writes().len(), 2);
            // Nothing more happens, of any kind, until the user resolves it.
            for flag in [NameFlag::Random, NameFlag::Keep] {
                let again = world.go(flag, true).await.unwrap_err();
                assert!(again.0.contains("--resolve"), "{again}");
            }
            let again = world.undo(true).await.unwrap_err();
            assert!(again.0.contains("did not confirm"), "{again}");
            assert_eq!(world.daemon.sim().writes().len(), 2);
            // The record keeps the names.
            let pending = world.journal().pending;
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].old_hostname.as_deref(), Some(""));
        }
        let world = World::new();
        world.daemon.sim().patch_answer = Some(Ok(403));
        let error = world.go(NameFlag::Random, true).await.unwrap_err();
        assert!(error.0.contains("Nothing was changed"), "{error}");
        assert_eq!(world.journal().last.unwrap().state, State::Refused);
    }

    #[tokio::test(start_paused = true)]
    async fn without_a_terminal_nothing_happens_without_explicit_consent() {
        let world = World::new();
        assert!(world.go(NameFlag::Ask, true).await.is_err());
        assert!(world.go(NameFlag::Keep, false).await.is_err());
        assert!(world.go(NameFlag::Random, false).await.is_err());
        assert!(world.daemon.sim().writes().is_empty());
        assert!(world.console.text().contains("permanently in public"));
    }

    #[tokio::test(start_paused = true)]
    async fn at_a_terminal_cancel_is_the_default() {
        let mut world = World::new();
        world.interactive = true;
        assert!(world.go(NameFlag::Ask, false).await.is_err());
        assert!(world.daemon.sim().writes().is_empty());
        let mut world = World::new();
        world.interactive = true;
        world.console = FakeConsole::answering(&[Choice::RandomName]);
        world.go(NameFlag::Ask, false).await.expect("setup");
        assert!(world.journal().rename.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn the_gateway_is_never_guessed_and_never_restarted() {
        for (listener, origin, word) in [
            (
                Listener::NotRunning,
                OriginAnswer::Current,
                "is not running",
            ),
            (
                Listener::Unidentified("421".into()),
                OriginAnswer::Current,
                "is not the LocalPilot gateway",
            ),
            (
                Listener::Gateway,
                OriginAnswer::OtherPolicy("x".into()),
                "older address",
            ),
            (
                Listener::Gateway,
                OriginAnswer::Unidentified("429".into()),
                "not confirmed",
            ),
        ] {
            let mut world = World::new();
            world.probes.listener = listener.clone();
            world.probes.origin = origin.clone();
            world
                .go(NameFlag::Keep, true)
                .await
                .expect("setup itself succeeded");
            assert!(
                world.console.text().contains(word),
                "{listener:?} {origin:?}"
            );
            assert_eq!(*world.probes.tls_calls.lock().unwrap(), 0);
            assert!(!world.journal().serve[0].verified);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn undo_removes_the_origin_then_restores_the_name() {
        let world = World::new();
        world.daemon.sim().config = json!({ "TCP": { "8080": { "HTTP": true } } });
        // The 8080 listener blocks a rename, so set up with the name kept,
        // then check the rename path separately.
        world.go(NameFlag::Keep, true).await.expect("setup");
        world.undo(true).await.expect("undo");
        assert_eq!(
            world.daemon.sim().config,
            json!({ "TCP": { "8080": { "HTTP": true } } })
        );
        assert!(world.journal().serve.is_empty());

        let world = World::new();
        world.go(NameFlag::Random, true).await.expect("setup");
        world.undo(true).await.expect("undo");
        let sim = world.daemon.sim();
        assert_eq!(sim.hostname, "", "the OS host name fallback is restored");
        assert_eq!(sim.config, json!({}));
        drop(sim);
        assert_eq!(world.journal().rename, None);
        assert!(world
            .console
            .text()
            .contains("advertised_url still names the old address"));
    }

    #[tokio::test(start_paused = true)]
    async fn undo_leaves_anything_changed_since_setup_alone() {
        let world = World::new();
        world.go(NameFlag::Keep, true).await.expect("setup");
        // Someone changed the origin's handler after setup.
        {
            let mut sim = world.daemon.sim();
            sim.config["Web"]["davids-desktop.tail1234.ts.net:443"]["Handlers"]["/"]["Proxy"] =
                json!("http://127.0.0.1:9999");
            sim.generation += 1;
        }
        let before = world.daemon.sim().writes().len();
        let error = world.undo(true).await.unwrap_err();
        assert!(
            error
                .0
                .contains("no longer exactly as LocalPilot set it up"),
            "{error}"
        );
        // Only the test save was sent; the record is kept.
        assert_eq!(world.daemon.sim().writes().len(), before + 1);
        assert!(!world.journal().serve.is_empty());

        // A name changed after setup is not restored.
        let world = World::new();
        world.go(NameFlag::Random, true).await.expect("setup");
        world.daemon.sim().hostname = "someone-else".into();
        world.daemon.sim().config = Value::Null;
        world.daemon.sim().generation += 1;
        let error = world.undo(true).await.unwrap_err();
        assert!(error.0.contains("leaves it as it is"), "{error}");
        assert!(world.journal().rename.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn a_journal_it_does_not_understand_refuses_before_any_request() {
        for text in [
            r#"{"version":1}"#,
            r#"{"version":3}"#,
            r#"{"version":2,"mystery":true}"#,
            "not json",
        ] {
            let world = World::new();
            std::fs::create_dir_all(world.state_dir()).unwrap();
            std::fs::write(journal_path(&world.state_dir()), text).unwrap();
            assert!(world.go(NameFlag::Keep, true).await.is_err(), "{text}");
            assert!(world.daemon.sim().requests.is_empty(), "{text}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn check_only_reads() {
        let world = World::new();
        check(&world.daemon, &world.console, PORT)
            .await
            .expect("check");
        let sim = world.daemon.sim();
        assert!(!sim.requests.is_empty());
        assert!(sim.writes().is_empty(), "{:?}", sim.requests);
    }

    #[tokio::test(start_paused = true)]
    async fn a_switch_of_node_after_consent_stops_before_any_write() {
        // The same name and settings, signed in as another node.
        let mut world = World::new();
        world.interactive = true;
        world.console = FakeConsole::answering(&[Choice::RandomName]);
        let daemon = &world.daemon;
        let switch = || daemon.sim().node_id = "nOTHER2CNTRL".into();
        // Switch while the prompt is open: the console answers, then the
        // node changes before setup reads again.
        struct Switching<'a, F: Fn() + Sync + Send> {
            inner: &'a FakeConsole,
            then: F,
        }
        impl<F: Fn() + Sync + Send> Console for Switching<'_, F> {
            fn say(&self, line: &str) {
                self.inner.say(line);
            }
            fn choose(&self, prompt: &str, choices: &[Choice]) -> Choice {
                let answer = self.inner.choose(prompt, choices);
                (self.then)();
                answer
            }
        }
        let console = Switching {
            inner: &world.console,
            then: switch,
        };
        let devices = || Ok(0);
        let random = |buffer: &mut [u8]| {
            buffer
                .iter_mut()
                .enumerate()
                .for_each(|(i, b)| *b = i as u8);
            Ok(())
        };
        let setup = Setup {
            daemon: &world.daemon,
            probes: &world.probes,
            console: &console,
            devices: &devices,
            state_dir: world.state_dir(),
            config_path: Some(world.config_path()),
            port: PORT,
            interactive: true,
            settle_within: SETTLE_WITHIN,
            persist: &write_journal,
            edit_config: &edit_config,
            random: &random,
        };
        let error = run(&setup, NameFlag::Ask, false).await.unwrap_err();
        assert!(error.0.contains("another node"), "{error}");
        assert!(world.daemon.sim().writes().is_empty());

        // Switch after the test save, before the rename: no PATCH.
        let world = World::new();
        world.daemon.sim().on_probe = Some(Box::new(|sim: &mut Sim| {
            sim.node_id = "nOTHER2CNTRL".into();
        }));
        let error = world.go(NameFlag::Random, true).await.unwrap_err();
        assert!(error.0.contains("another node"), "{error}");
        let writes = world.daemon.sim().writes();
        assert_eq!(writes.len(), 1, "only the test save: {writes:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn undo_writes_only_on_a_supported_understood_tagged_install() {
        for spoil in [
            |sim: &mut Sim| sim.version = "1.104.5-dev20261103-t73a48057a".into(),
            |sim: &mut Sim| {
                sim.config["Mystery"] = json!(1);
            },
            |sim: &mut Sim| sim.node_id = "nOTHER2CNTRL".into(),
        ] {
            let world = World::new();
            world.go(NameFlag::Keep, true).await.expect("setup");
            let before = world.daemon.sim().writes().len();
            spoil(&mut world.daemon.sim());
            assert!(world.undo(true).await.is_err());
            assert_eq!(world.daemon.sim().writes().len(), before, "no POST at all");
            assert!(!world.journal().serve.is_empty(), "the record is kept");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_origin_that_becomes_free_is_written_only_after_a_test_save() {
        let world = World::new();
        // At the start the origin is someone else's identical entry...
        world.daemon.sim().config = ours("davids-desktop.tail1234.ts.net");
        // ...which is gone by the time setup reads again.
        let mut world = world;
        world.interactive = true;
        world.console = FakeConsole::answering(&[Choice::Proceed]);
        let console = Removing {
            inner: &world.console,
            daemon: &world.daemon,
        };
        struct Removing<'a> {
            inner: &'a FakeConsole,
            daemon: &'a Fake,
        }
        impl Console for Removing<'_> {
            fn say(&self, line: &str) {
                self.inner.say(line);
            }
            fn choose(&self, prompt: &str, choices: &[Choice]) -> Choice {
                let mut sim = self.daemon.sim();
                sim.config = json!({ "TCP": { "8080": { "HTTP": true } } });
                sim.generation += 1;
                drop(sim);
                self.inner.choose(prompt, choices)
            }
        }
        let devices = || Ok(0);
        let random = |buffer: &mut [u8]| {
            buffer
                .iter_mut()
                .enumerate()
                .for_each(|(i, b)| *b = i as u8);
            Ok(())
        };
        let setup = Setup {
            daemon: &world.daemon,
            probes: &world.probes,
            console: &console,
            devices: &devices,
            state_dir: world.state_dir(),
            config_path: Some(world.config_path()),
            port: PORT,
            interactive: true,
            settle_within: SETTLE_WITHIN,
            persist: &write_journal,
            edit_config: &edit_config,
            random: &random,
        };
        run(&setup, NameFlag::Keep, false).await.expect("setup");
        let sim = world.daemon.sim();
        let writes = sim.writes();
        assert!(
            writes[0].starts_with("POST /localapi/v0/serve-config lp-probe-"),
            "{writes:?}"
        );
        // The test save carried the settings as they were after the prompt,
        // not the ones seen before it.
        assert_eq!(
            sim.bodies[0],
            json!({ "TCP": { "8080": { "HTTP": true } } })
        );
        assert!(
            writes[1].starts_with("POST /localapi/v0/serve-config e"),
            "{writes:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn resolving_forgets_without_adopting() {
        let world = World::new();
        world.daemon.sim().patch_answer = Some(Err(ApiFailure::Interrupted("timed out".into())));
        assert!(world.go(NameFlag::Random, true).await.is_err());
        world.resolve(true).await.expect("resolve");
        let journal = world.journal();
        assert!(journal.pending.is_empty());
        assert!(journal.rename.is_none(), "nothing is adopted");
        // Setup can run again afterwards.
        world.go(NameFlag::Keep, true).await.expect("setup");
    }

    #[tokio::test(start_paused = true)]
    async fn an_earlier_route_stays_recorded_when_the_name_moves() {
        let world = World::new();
        world.go(NameFlag::Keep, true).await.expect("setup");
        // The computer is renamed outside LocalPilot; its old route stays,
        // and port 443 is shared by every name, so no second one is added.
        {
            let mut sim = world.daemon.sim();
            sim.name = "renamed-pc".into();
            sim.certs.push("renamed-pc".into());
        }
        let error = world.go(NameFlag::Keep, true).await.unwrap_err();
        assert!(
            error.0.contains("port 443 already has a Serve setting"),
            "{error}"
        );
        let names: Vec<String> = world
            .journal()
            .serve
            .iter()
            .map(|o| o.name.clone())
            .collect();
        assert_eq!(names, vec!["davids-desktop.tail1234.ts.net".to_string()]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_after_a_change_says_what_was_done() {
        let world = World::new();
        std::fs::create_dir_all(world.config_path().parent().unwrap()).unwrap();
        std::fs::write(world.config_path(), "model = [unclosed\n").unwrap();
        let error = world.go(NameFlag::Keep, true).await.unwrap_err();
        assert!(
            error.0.contains("Done before this: Tailscale now serves"),
            "{error}"
        );
        assert!(!error.0.contains("Nothing was changed"), "{error}");
        // And the consent stated the test save's limit.
        assert!(world
            .console
            .text()
            .contains("could undo a change made elsewhere"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_config_changed_right_after_its_replace_is_reported_as_changed() {
        let mut world = World::new();
        world.edit_config = Box::new(|path, origin| {
            edit_config_with(path, origin, &|written| {
                std::fs::write(written, "someone = \"else\"\n").unwrap();
            })
        });
        let error = world.go(NameFlag::Keep, true).await.unwrap_err();
        assert!(error.0.contains("Setup replaced your config"), "{error}");
        assert!(
            error.0.contains("Done before this: Tailscale now serves"),
            "{error}"
        );
        assert!(!error.0.contains("not updated"), "{error}");
        assert!(!error.0.contains("Nothing was changed"), "{error}");

        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("config.toml");
        // A read-back failure after the replace is "changed, not confirmed".
        assert!(matches!(
            edit_config_with(Some(&path), "https://x", &|written| {
                std::fs::remove_file(written).unwrap();
            }),
            Err(ConfigError::After(_))
        ));
        // A refusal before it is "nothing replaced".
        std::fs::write(
            &path,
            "model = [unclosed
",
        )
        .unwrap();
        assert!(matches!(
            edit_config_with(Some(&path), "https://x", &|_| {}),
            Err(ConfigError::Before(_))
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn undo_sends_nothing_on_an_unknown_setting_after_a_conflict() {
        let world = World::new();
        world.go(NameFlag::Keep, true).await.expect("setup");
        let before = world.daemon.sim().writes().len();
        {
            let mut sim = world.daemon.sim();
            sim.config["Foreground"] = json!({ "first": { "TCP": { "443": { "HTTPS": true } } } });
            sim.config["Services"] = json!({ "svc:later": { "UnknownExposure": true } });
            sim.generation += 1;
        }
        assert!(world.undo(true).await.is_err());
        assert_eq!(world.daemon.sim().writes().len(), before, "no POST at all");
    }

    #[tokio::test(start_paused = true)]
    async fn without_random_bytes_nothing_is_named_or_sent() {
        let mut world = World::new();
        world.random_fails = true;
        let error = world.go(NameFlag::Random, true).await.unwrap_err();
        assert!(error.0.contains("random source failed"), "{error}");
        assert!(world.daemon.sim().writes().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn the_journal_is_private_even_over_a_broad_leftover() {
        use std::os::unix::fs::PermissionsExt as _;
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("remote");
        std::fs::create_dir_all(&dir).unwrap();
        let leftover = dir.join("setup.json.tmp");
        std::fs::write(&leftover, "x").unwrap();
        std::fs::set_permissions(&leftover, std::fs::Permissions::from_mode(0o666)).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        write_journal(&dir, &Journal::default()).unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&journal_path(&dir)), 0o600);
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn the_config_edit_keeps_everything_else_and_refuses_what_it_cannot_parse() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("config.toml");
        let original = "# my settings\nmodel = \"x\" # keep me\n\n[remote]\nport = 47610\n";
        std::fs::write(&path, original).unwrap();
        assert_eq!(
            edit_config(Some(&path), "https://pc.tail1234.ts.net"),
            Ok(ConfigEdit::Written)
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("# my settings\nmodel = \"x\" # keep me\n"),
            "{text}"
        );
        assert!(text.contains("port = 47610"));
        assert!(text.contains("enabled = true"));
        assert!(text.contains("advertised_url = \"https://pc.tail1234.ts.net\""));
        assert_eq!(
            edit_config(Some(&path), "https://pc.tail1234.ts.net"),
            Ok(ConfigEdit::Unchanged)
        );

        std::fs::write(&path, "model = [unclosed\n").unwrap();
        assert!(edit_config(Some(&path), "https://x").is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "model = [unclosed\n"
        );
        std::fs::write(&path, "remote = 3\n").unwrap();
        assert!(edit_config(Some(&path), "https://x").is_err());
    }
}
