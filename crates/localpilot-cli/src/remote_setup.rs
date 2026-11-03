//! `localpilot remote setup`: give the phone an HTTPS address through
//! Tailscale on this computer, changing only what setup adds and recording
//! only what it can confirm.
//!
//! A run reads first and refuses before any change when anything is unclear.
//! Then, in order:
//! 1. a test save Tailscale must refuse (a deliberately stale version tag),
//!    which shows its conditional write works on this install;
//! 2. optionally, a random name for this computer, offered because the
//!    certificate publishes the name;
//! 3. the phone origin, written only on the settings it was planned on;
//! 4. the gateway address in the user config;
//! 5. checks of the gateway and of the address from this computer.
//!
//! The journal keeps what LocalPilot has confirmed it owns apart from the
//! newest operation's result; an attempt whose outcome is unknown confers
//! nothing, and no later refusal erases an earlier confirmed record.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::remote_gateway::{Listener, OriginAnswer};
use crate::remote_tailscale::{
    eligible, name_in_use, one_label, origin_plan, read_node, version_tag, without_ours,
    ApiFailure, Node, OriginPlan, Refusal, Reply,
};

const STATUS: &str = "/localapi/v0/status?peers=false";
const SERVE: &str = "/localapi/v0/serve-config";
const PREFS: &str = "/localapi/v0/prefs";
/// How long a new name may take to reach this computer's status.
pub(crate) const SETTLE_WITHIN: Duration = Duration::from_secs(30);
const SETTLE_EVERY: Duration = Duration::from_secs(1);
const JOURNAL_VERSION: u32 = 1;

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
    /// Random bytes for the name and the test tag.
    pub random: &'a (dyn Fn(&mut [u8]) + Sync),
}

/// Why a run stopped. The message says what was and was not changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stop(pub String);

impl std::fmt::Display for Stop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn stop<T>(message: impl Into<String>) -> Result<T, Stop> {
    Err(Stop(message.into()))
}

fn refused<T>(refusal: &Refusal) -> Result<T, Stop> {
    stop(format!("{}. {}", refusal.message(), refusal.remedy()))
}

// ---------------------------------------------------------------- journal

/// What setup knows about its own work on this computer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Journal {
    pub version: u32,
    /// The phone origin LocalPilot added and confirmed.
    #[serde(default)]
    pub serve: Option<ServeOwned>,
    /// The name LocalPilot gave this computer and confirmed.
    #[serde(default)]
    pub rename: Option<RenameOwned>,
    /// The newest operation's result, whatever it was.
    #[serde(default)]
    pub last: Option<Attempt>,
}

impl Default for Journal {
    fn default() -> Self {
        Self {
            version: JOURNAL_VERSION,
            serve: None,
            rename: None,
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
    pub step: Step,
    pub state: State,
    pub node_id: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub old_hostname: Option<String>,
    #[serde(default)]
    pub new_hostname: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Step {
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

fn load_journal(dir: &Path) -> Result<Journal, Stop> {
    let path = journal_path(dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Journal::default()),
        Err(error) => return stop(format!("could not read {}: {error}", path.display())),
    };
    let journal: Journal = serde_json::from_str(&text).map_err(|error| {
        Stop(format!(
            "{} is not a record this version understands ({error}); nothing was changed. \
             Move it aside only after checking your Tailscale settings.",
            path.display()
        ))
    })?;
    if journal.version != JOURNAL_VERSION {
        return stop(format!(
            "{} was written by another version of LocalPilot; nothing was changed",
            path.display()
        ));
    }
    Ok(journal)
}

/// Write the journal atomically, readable only by this user.
pub(crate) fn write_journal(dir: &Path, journal: &Journal) -> std::io::Result<()> {
    use std::io::Write as _;
    std::fs::create_dir_all(dir)?;
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
        file.write_all(&text)?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, journal_path(dir))
}

/// The run's exclusive hold on setup.
struct Held {
    _lock: Box<fd_lock::RwLock<std::fs::File>>,
}

fn hold(dir: &Path) -> Result<Held, Stop> {
    std::fs::create_dir_all(dir)
        .map_err(|error| Stop(format!("could not create {}: {error}", dir.display())))?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("setup.lock"))
        .map_err(|error| Stop(format!("could not open the setup lock: {error}")))?;
    let mut lock = Box::new(fd_lock::RwLock::new(file));
    // The guard is forgotten so the lock stays taken until `Held` closes the
    // file at the end of the run.
    match lock.try_write() {
        Ok(guard) => std::mem::forget(guard),
        Err(_) => return stop("another `localpilot remote setup` is running"),
    }
    Ok(Held { _lock: lock })
}

impl Setup<'_> {
    fn save(&self, journal: &Journal) -> Result<(), String> {
        (self.persist)(&self.state_dir, journal).map_err(|error| error.to_string())
    }

    /// Record `attempt` as the newest result before acting on it. Failing to
    /// record means not acting.
    fn plan(&self, journal: &mut Journal, attempt: Attempt) -> Result<(), Stop> {
        journal.last = Some(attempt);
        self.save(journal).map_err(|error| {
            Stop(format!(
                "LocalPilot could not record what it was about to do ({error}); nothing more was changed"
            ))
        })
    }

    /// Record the newest result after the fact; failure is reported but the
    /// caller decides whether to go on.
    fn settle_attempt(
        &self,
        journal: &mut Journal,
        state: State,
        detail: &str,
    ) -> Result<(), String> {
        if let Some(last) = journal.last.as_mut() {
            last.state = state;
            last.detail = detail.to_string();
        }
        self.save(journal)
    }

    fn random_hex(&self, bytes: usize) -> String {
        let mut buffer = vec![0u8; bytes];
        (self.random)(&mut buffer);
        buffer.iter().fold(String::new(), |mut text, byte| {
            use std::fmt::Write as _;
            let _ = write!(text, "{byte:02x}");
            text
        })
    }

    fn random_name(&self) -> String {
        const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
        let mut buffer = [0u8; 6];
        (self.random)(&mut buffer);
        let label: String = buffer
            .iter()
            .map(|byte| ALPHABET[usize::from(*byte) % ALPHABET.len()] as char)
            .collect();
        format!("lp-{label}")
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

async fn document(
    daemon: &dyn Daemon,
    path: &str,
    what: &str,
) -> Result<(Value, Option<String>), Stop> {
    let reply = daemon
        .get(path)
        .await
        .map_err(|failure| Stop(format!("{failure}; nothing was changed")))?;
    match reply.status {
        200 => {}
        403 => {
            return stop(format!(
                "this user may not read Tailscale's {what}; nothing was changed. On Linux, \
                 `sudo tailscale set --operator=$USER` lets your user manage Tailscale."
            ))
        }
        status => {
            let detail: String = String::from_utf8_lossy(&reply.body)
                .trim()
                .chars()
                .take(200)
                .collect();
            return stop(format!(
                "Tailscale answered {status} when asked for its {what}: {detail}; nothing was changed"
            ));
        }
    }
    if reply.truncated {
        return stop(format!(
            "Tailscale's {what} was larger than setup reads; nothing was changed"
        ));
    }
    let value = serde_json::from_slice(&reply.body).map_err(|error| {
        Stop(format!(
            "Tailscale's {what} was not JSON ({error}); nothing was changed"
        ))
    })?;
    Ok((value, reply.etag))
}

async fn read(daemon: &dyn Daemon) -> Result<Reading, Stop> {
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

async fn hostname(daemon: &dyn Daemon) -> Result<String, Stop> {
    let (prefs, _) = document(daemon, PREFS, "preferences").await?;
    prefs
        .get("Hostname")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            Stop("Tailscale's preferences have no name setting; nothing was changed".to_string())
        })
}

fn same_node(a: &Node, node_id: &str, tailnet: &str) -> bool {
    a.node_id == node_id && a.tailnet == tailnet
}

/// The status, with the node identity and name checked against what the run
/// started on.
fn expect_node(reading: &Reading, start: &Node, name: &str) -> Result<(), Stop> {
    if !same_node(&reading.node, &start.node_id, &start.tailnet) {
        return stop(
            "Tailscale is now signed in as another node or tailnet than when setup started; \
             nothing more was changed",
        );
    }
    if !reading.node.name.eq_ignore_ascii_case(name) {
        return stop(format!(
            "this computer's Tailscale name is now {} instead of {name}; nothing more was changed",
            reading.node.name
        ));
    }
    Ok(())
}

// ------------------------------------------------------------- check

/// `localpilot remote setup --check`: what setup would do, by GET only.
pub(crate) async fn check(
    daemon: &dyn Daemon,
    console: &dyn Console,
    port: u16,
) -> Result<(), Stop> {
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
                "Setup would serve {}/ from the gateway on 127.0.0.1:{port}, changing nothing \
                 else in your Serve settings.",
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
            console.say("This check changed nothing. Run `localpilot remote setup` to set it up.");
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
}

// ------------------------------------------------------------- setup

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
             Certificate Transparency logs, including the tailnet part {suffix}. Names in \
             earlier certificates cannot be removed."
        ),
        "First, setup sends a test save that Tailscale must refuse, to confirm it never \
         saves over settings that changed in between."
            .to_string(),
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
            "A neutral name reduces what future certificates reveal about this computer; \
             it does not make it anonymous."
                .to_string(),
        );
    }
    lines
}

/// Show `lines`, then go ahead only with `--yes` or a yes at the prompt.
fn consent(s: &Setup<'_>, lines: &[String], yes: bool) -> Result<(), Stop> {
    for line in lines {
        s.console.say(line);
    }
    if yes {
        return Ok(());
    }
    if !s.interactive {
        return stop("Nothing was changed. Without a terminal to ask in, add --yes to confirm.");
    }
    if s.console
        .choose("Go ahead?", &[Choice::Proceed, Choice::Cancel])
        == Choice::Proceed
    {
        Ok(())
    } else {
        stop("Cancelled; nothing was changed.")
    }
}

/// Whether a rename can be offered, and if not, why.
enum Rename {
    Offer { proposed: String, devices: usize },
    No(String),
}

/// `localpilot remote setup`.
pub(crate) async fn run(s: &Setup<'_>, flag: NameFlag, yes: bool) -> Result<(), Stop> {
    let _held = hold(&s.state_dir)?;
    let mut journal = load_journal(&s.state_dir)?;
    let pending_rename = journal.last.as_ref().is_some_and(|last| {
        last.step == Step::Rename && matches!(last.state, State::Planned | State::Unresolved)
    });
    if let Some(last) = &journal.last {
        if matches!(
            last.state,
            State::Planned | State::Unresolved | State::Unexpected
        ) {
            s.console.say(&format!(
                "Note: an earlier run ended without a confirmed result ({:?}, {:?}: {}). \
                 LocalPilot does not treat that as its own.",
                last.step, last.state, last.detail
            ));
        }
    }

    // Read, and refuse before any change.
    let start = read(s.daemon).await?;
    let node = eligible(&start.status).or_else(|refusal| refused(&refusal))?;
    version_tag(start.etag.as_deref()).or_else(|refusal| refused(&refusal))?;
    let plan_now =
        origin_plan(&start.config, &node.name, s.port).or_else(|refusal| refused(&refusal))?;
    let start_hostname = hostname(s.daemon).await?;
    for owner in [
        journal.serve.as_ref().map(|o| (&o.node_id, &o.tailnet)),
        journal.rename.as_ref().map(|o| (&o.node_id, &o.tailnet)),
    ]
    .into_iter()
    .flatten()
    {
        if !same_node(&node, owner.0, owner.1) {
            return stop(
                "LocalPilot's earlier setup was made on another Tailscale node or tailnet; \
                 nothing was changed. Run `localpilot remote setup --undo` while signed in as \
                 that one first.",
            );
        }
    }

    let rename = if journal.rename.is_some() {
        Rename::No("this computer already has the name LocalPilot gave it".to_string())
    } else if pending_rename {
        Rename::No("an earlier rename has no confirmed result".to_string())
    } else if let Some(use_) = name_in_use(&start.config, &node.name) {
        Rename::No(format!("{use_}, and a new name would move it"))
    } else {
        let proposed = format!("{}.{}", s.random_name(), node.suffix);
        let free = one_label(&proposed, &node.suffix)
            && matches!(
                origin_plan(&start.config, &proposed, s.port),
                Ok(OriginPlan::Free(_))
            );
        match (s.devices)() {
            Err(error) => Rename::No(format!(
                "the list of paired devices could not be read ({error})"
            )),
            Ok(_) if !free => Rename::No(format!("{proposed} is not free")),
            Ok(devices) => Rename::Offer { proposed, devices },
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
                    return stop(format!(
                        "A new name cannot be given: {why}. Nothing was changed."
                    ))
                }
                (None, Rename::Offer { .. }) => return stop("Nothing was changed."),
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
                return stop(
                    "Nothing was changed. Without a terminal to ask in, run again with --yes \
                     and either --random-name or --keep-name.",
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
                _ => return stop("Cancelled; nothing was changed."),
            }
        }
    };
    let target_name = match (&offer, renaming) {
        (Some((proposed, _)), true) => proposed.clone(),
        _ => node.name.clone(),
    };
    let writes = renaming || matches!(plan_now, OriginPlan::Free(_));
    if writes {
        probe(s, &mut journal, &start, &node).await?;
    }

    if renaming {
        rename_node(s, &mut journal, &node, &start_hostname, &target_name).await?;
    }

    // The phone origin, on a fresh reading.
    let fresh = read(s.daemon).await?;
    expect_node(&fresh, &node, &target_name)?;
    let fresh_node = eligible(&fresh.status).or_else(|refusal| refused(&refusal))?;
    let origin = format!("https://{}", fresh_node.name);
    let owned_here = journal.serve.as_ref().is_some_and(|owned| {
        owned.name.eq_ignore_ascii_case(&fresh_node.name) && owned.port == s.port
    });
    match origin_plan(&fresh.config, &fresh_node.name, s.port)
        .or_else(|refusal| refused(&refusal))?
    {
        OriginPlan::AlreadyServed if owned_here => {
            s.console
                .say(&format!("{origin}/ is set up by LocalPilot already."));
        }
        OriginPlan::AlreadyServed => {
            s.console.say(&format!(
                "{origin}/ already forwards to the gateway, but LocalPilot has no \
                 confirmed record of setting it up; it uses it and will never remove it."
            ));
        }
        OriginPlan::Free(planned) => {
            let tag = version_tag(fresh.etag.as_deref()).or_else(|refusal| refused(&refusal))?;
            serve(s, &mut journal, &fresh_node, &planned, &tag).await?;
        }
    }

    // The user config.
    match edit_config(s.config_path.as_deref(), &origin) {
        Ok(true) => s.console.say(&format!(
            "Your config now has [remote] enabled = true and advertised_url = \"{origin}\"."
        )),
        Ok(false) => {}
        Err(error) => {
            return stop(format!(
                "Tailscale is set up, but your config was not updated: {error}. Set [remote] \
                 enabled = true and advertised_url = \"{origin}\" yourself."
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
    match s.probes.origin(s.port, &fresh_node.name).await {
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
    let before_tls = read(s.daemon).await?;
    expect_node(&before_tls, &node, &fresh_node.name)?;
    eligible(&before_tls.status).or_else(|refusal| refused(&refusal))?;
    if origin_plan(&before_tls.config, &fresh_node.name, s.port)
        .or_else(|refusal| refused(&refusal))?
        != OriginPlan::AlreadyServed
    {
        return stop("Tailscale's Serve settings changed before the check; it was not run.");
    }
    s.probes.tls(&origin).await.map_err(|why| {
        Stop(format!(
            "The check of {origin} from this computer failed: {why}."
        ))
    })?;
    if let Some(owned) = journal.serve.as_mut() {
        if !owned.verified {
            owned.verified = true;
            if let Err(error) = s.save(&journal) {
                return stop(format!(
                    "{origin} works from this computer, but LocalPilot could not record that ({error})."
                ));
            }
        }
    }
    s.console.say(&format!(
        "Verified from this computer; the phone is not checked yet. Pair it with \
         `localpilot devices pair` and open {origin} on the phone."
    ));
    Ok(())
}

/// The test save: the settings exactly as read, with a version tag that is
/// deliberately not theirs. Only a refusal passes.
async fn probe(
    s: &Setup<'_>,
    journal: &mut Journal,
    start: &Reading,
    node: &Node,
) -> Result<(), Stop> {
    let observed = start.etag.clone().unwrap_or_default();
    let mut tag = format!("lp-probe-{}", s.random_hex(8));
    while tag == observed {
        tag = format!("lp-probe-{}", s.random_hex(8));
    }
    let body = if start.config.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        start.config.clone()
    };
    s.plan(
        journal,
        Attempt {
            step: Step::Probe,
            state: State::Planned,
            node_id: node.node_id.clone(),
            detail: String::new(),
            old_hostname: None,
            new_hostname: None,
        },
    )?;
    let (state, message) = match s.daemon.post(SERVE, &body, &tag).await {
        Ok(reply) if reply.status == 412 => (State::Passed, None),
        Ok(reply) if reply.status == 200 => (
            State::Unexpected,
            Some(
                "Tailscale accepted a save it should have refused, so on this install it does \
                 not protect settings that changed in between. It saved your settings exactly \
                 as they were just read; nothing else was changed."
                    .to_string(),
            ),
        ),
        Ok(reply) if matches!(reply.status, 401 | 403) => (
            State::Refused,
            Some(format!(
                "Tailscale refused to let this user change its settings ({}); nothing was \
                 changed. On Linux, `sudo tailscale set --operator=$USER` lets your user manage \
                 Tailscale.",
                reply.status
            )),
        ),
        Ok(reply) => (
            State::Unresolved,
            Some(format!(
                "Tailscale answered the test save with {}, which setup does not expect; nothing \
                 more was changed. Check `tailscale serve status`.",
                reply.status
            )),
        ),
        Err(failure) => (
            State::Unresolved,
            Some(format!(
                "{failure} during the test save; nothing more was changed. Check `tailscale \
                 serve status`."
            )),
        ),
    };
    let detail = message.clone().unwrap_or_default();
    let recorded = s.settle_attempt(journal, state, &detail);
    if let Some(message) = message {
        return stop(message);
    }
    recorded.map_err(|error| {
        Stop(format!(
            "LocalPilot could not record the test save ({error}); nothing was changed"
        ))
    })
}

async fn rename_node(
    s: &Setup<'_>,
    journal: &mut Journal,
    node: &Node,
    old: &str,
    target: &str,
) -> Result<(), Stop> {
    let label = target.split('.').next().unwrap_or_default().to_string();
    if hostname(s.daemon).await? != old {
        return stop(
            "Tailscale's name setting changed while setting up; nothing more was changed.",
        );
    }
    s.plan(
        journal,
        Attempt {
            step: Step::Rename,
            state: State::Planned,
            node_id: node.node_id.clone(),
            detail: String::new(),
            old_hostname: Some(old.to_string()),
            new_hostname: Some(label.clone()),
        },
    )?;
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
        let (state, message) = match &answered {
            Ok(reply) if reply.status == 403 => (
                State::Refused,
                "Tailscale refused to let this user rename the computer; nothing was changed."
                    .to_string(),
            ),
            Ok(reply) => (
                State::Unresolved,
                format!(
                    "Tailscale's answer to the rename ({}) could not be confirmed, so whether \
                     this computer was renamed to {label} is not known. Nothing more was \
                     changed; check the name with `tailscale status`.",
                    reply.status
                ),
            ),
            Err(failure) => (
                State::Unresolved,
                format!(
                    "{failure} during the rename, so whether this computer was renamed to \
                     {label} is not known. Nothing more was changed; check the name with \
                     `tailscale status`."
                ),
            ),
        };
        let _ = s.settle_attempt(journal, state, &message);
        return stop(message);
    }
    journal.rename = Some(RenameOwned {
        node_id: node.node_id.clone(),
        tailnet: node.tailnet.clone(),
        old_hostname: old.to_string(),
        new_hostname: label.clone(),
    });
    if let Err(error) = s.settle_attempt(journal, State::Done, "") {
        return stop(format!(
            "Tailscale renamed this computer to {label}, but LocalPilot could not record it \
             ({error}); nothing else was done. Restore the old name in Tailscale yourself if \
             you want it back."
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
                    return stop(
                        "Tailscale is now signed in as another node or tailnet; nothing more \
                         was changed.",
                    );
                }
                observed.clone_from(&now.name);
                if now.name.eq_ignore_ascii_case(target) && eligible(&status).is_ok() {
                    return Ok(());
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return stop(format!(
                "Tailscale now reports this computer as {observed}, not yet {target} with a \
                 certificate. It may still be updating, may have added a suffix to avoid a \
                 clash, or the name may be set in the admin console. Nothing more was changed; \
                 run `localpilot remote setup` again in a minute, or check the name in the \
                 Tailscale admin console."
            ));
        }
    }
}

async fn serve(
    s: &Setup<'_>,
    journal: &mut Journal,
    node: &Node,
    planned: &Value,
    tag: &str,
) -> Result<(), Stop> {
    s.plan(
        journal,
        Attempt {
            step: Step::Serve,
            state: State::Planned,
            node_id: node.node_id.clone(),
            detail: String::new(),
            old_hostname: None,
            new_hostname: None,
        },
    )?;
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
                     up as planned, so LocalPilot does not claim it. Nothing more was changed; \
                     check `tailscale serve status`."
                );
                let _ = s.settle_attempt(journal, State::Unresolved, &message);
                return stop(message);
            }
            journal.serve = Some(ServeOwned {
                node_id: node.node_id.clone(),
                tailnet: node.tailnet.clone(),
                name: node.name.clone(),
                port: s.port,
                verified: false,
            });
            if let Err(error) = s.settle_attempt(journal, State::Done, "") {
                return stop(format!(
                    "Tailscale now serves {origin}, but LocalPilot could not record it ({error}); \
                     nothing else was done."
                ));
            }
            s.console
                .say(&format!("Tailscale now serves {origin}/ from the gateway."));
            Ok(())
        }
        Ok(reply) => {
            let (state, message) = match reply.status {
                412 => (
                    State::Refused,
                    "Tailscale's Serve settings changed while setting up; nothing was changed. \
                     Run setup again."
                        .to_string(),
                ),
                401 | 403 => (
                    State::Refused,
                    format!(
                        "Tailscale refused the change ({}); nothing was changed.{}",
                        reply.status,
                        crate::remote_tailscale::admin_note(planned)
                            .map(|note| format!(" Note: {note}."))
                            .unwrap_or_default()
                    ),
                ),
                status => (
                    State::Unresolved,
                    format!(
                        "Tailscale answered {status}, so whether {origin} was set up is not \
                         known. Nothing more was changed; check `tailscale serve status`."
                    ),
                ),
            };
            let _ = s.settle_attempt(journal, state, &message);
            stop(message)
        }
        Err(failure) => {
            let message = format!(
                "{failure} while setting up {origin}, so whether it was set up is not known. \
                 Nothing more was changed; check `tailscale serve status`."
            );
            let _ = s.settle_attempt(journal, State::Unresolved, &message);
            stop(message)
        }
    }
}

// ------------------------------------------------------------- config

/// Set `[remote] enabled = true` and `advertised_url` in the user config,
/// keeping everything else byte for byte. Every LocalPilot writer of this
/// file takes the same exclusive lock; an editor outside LocalPilot that
/// writes between the last check and the replace is not detected. Returns
/// whether the file changed.
pub(crate) fn edit_config(path: Option<&Path>, origin: &str) -> Result<bool, String> {
    let path = path.ok_or("there is no user config location on this system")?;
    let directory = path.parent().ok_or("the config path has no folder")?;
    std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let lock_path = path.with_extension("toml.lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|error| format!("could not open {}: {error}", lock_path.display()))?;
    let mut lock = fd_lock::RwLock::new(lock_file);
    let _guard = lock
        .try_write()
        .map_err(|_| "another LocalPilot command is writing the config".to_string())?;

    let before = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    let text =
        String::from_utf8(before.clone()).map_err(|_| "the config is not UTF-8".to_string())?;
    let mut document: toml_edit::DocumentMut = text
        .parse()
        .map_err(|error| format!("the config is not valid TOML ({error})"))?;
    let remote = document
        .entry("remote")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
    let table = remote
        .as_table_mut()
        .ok_or("[remote] in the config is not a table")?;
    table["enabled"] = toml_edit::value(true);
    table["advertised_url"] = toml_edit::value(origin);
    let after = document.to_string();
    if after.as_bytes() == before.as_slice() {
        return Ok(false);
    }
    // Replace only what was read.
    let now = std::fs::read(path).unwrap_or_default();
    if now != before {
        return Err("the config changed while setup was editing it".to_string());
    }
    let temporary = path.with_extension("toml.tmp");
    std::fs::write(&temporary, after.as_bytes()).map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, path).map_err(|error| error.to_string())?;
    let written = std::fs::read(path).map_err(|error| error.to_string())?;
    if written != after.as_bytes() {
        return Err("the config changed right after setup wrote it".to_string());
    }
    Ok(true)
}

// ------------------------------------------------------------- undo

/// `localpilot remote setup --undo`: remove what LocalPilot confirmed it set
/// up, each part only while it is still exactly as recorded.
pub(crate) async fn undo(s: &Setup<'_>, yes: bool) -> Result<(), Stop> {
    let _held = hold(&s.state_dir)?;
    let mut journal = load_journal(&s.state_dir)?;
    if journal.serve.is_none() && journal.rename.is_none() {
        s.console
            .say("LocalPilot has not set anything up in Tailscale on this computer.");
        return Ok(());
    }
    let start = read(s.daemon).await?;
    for owner in [
        journal.serve.as_ref().map(|o| (&o.node_id, &o.tailnet)),
        journal.rename.as_ref().map(|o| (&o.node_id, &o.tailnet)),
    ]
    .into_iter()
    .flatten()
    {
        if !same_node(&start.node, owner.0, owner.1) {
            return stop(
                "Tailscale is signed in as another node or tailnet than the one LocalPilot set \
                 up; nothing was changed.",
            );
        }
    }
    let mut lines = Vec::new();
    if let Some(owned) = &journal.serve {
        lines.push(format!(
            "Undo removes https://{}/ from Tailscale Serve.",
            owned.name
        ));
    }
    if let Some(owned) = &journal.rename {
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
    lines.push("First, setup sends a test save that Tailscale must refuse.".to_string());
    for line in &lines {
        s.console.say(line);
    }
    if !yes {
        if !s.interactive {
            return stop(
                "Nothing was changed. Without a terminal to ask in, add --yes to confirm.",
            );
        }
        if s.console
            .choose("Undo?", &[Choice::Proceed, Choice::Cancel])
            != Choice::Proceed
        {
            return stop("Cancelled; nothing was changed.");
        }
    }
    probe(s, &mut journal, &start, &start.node).await?;

    if let Some(owned) = journal.serve.clone() {
        let fresh = read(s.daemon).await?;
        match origin_plan(&fresh.config, &owned.name, owned.port) {
            Ok(OriginPlan::Free(_)) => {
                s.console
                    .say(&format!("https://{}/ was already removed.", owned.name));
                journal.serve = None;
                s.save(&journal)
                    .map_err(|error| Stop(format!("could not record the removal ({error})")))?;
            }
            Ok(OriginPlan::AlreadyServed) => {
                let tag =
                    version_tag(fresh.etag.as_deref()).or_else(|refusal| refused(&refusal))?;
                let without = without_ours(&fresh.config, &owned.name);
                s.plan(
                    &mut journal,
                    Attempt {
                        step: Step::UndoServe,
                        state: State::Planned,
                        node_id: owned.node_id.clone(),
                        detail: String::new(),
                        old_hostname: None,
                        new_hostname: None,
                    },
                )?;
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
                    let (state, message) = match &answered {
                        Ok(reply) if matches!(reply.status, 401 | 403 | 412) => (
                            State::Refused,
                            format!(
                                "Tailscale did not remove https://{}/ ({}); nothing was changed. \
                                 Run undo again.",
                                owned.name, reply.status
                            ),
                        ),
                        _ => (
                            State::Unresolved,
                            format!(
                                "whether https://{}/ was removed is not known; LocalPilot keeps \
                                 its record. Check `tailscale serve status`.",
                                owned.name
                            ),
                        ),
                    };
                    let _ = s.settle_attempt(&mut journal, state, &message);
                    return stop(message);
                }
                journal.serve = None;
                if let Err(error) = s.settle_attempt(&mut journal, State::Done, "") {
                    return stop(format!(
                        "https://{}/ was removed, but LocalPilot could not record it ({error}).",
                        owned.name
                    ));
                }
                s.console.say(&format!(
                    "Removed https://{}/ from Tailscale Serve.",
                    owned.name
                ));
            }
            Err(refusal) => {
                return stop(format!(
                    "https://{}/ is no longer exactly as LocalPilot set it up ({}); it was not \
                     removed.",
                    owned.name,
                    refusal.message()
                ))
            }
        }
    }

    if let Some(owned) = journal.rename.clone() {
        let fresh = read(s.daemon).await?;
        if let Some(use_) = name_in_use(&fresh.config, &fresh.node.name) {
            return stop(format!(
                "{use_}, and restoring the old name would move it; the name was not changed."
            ));
        }
        if hostname(s.daemon).await? != owned.new_hostname {
            return stop(
                "Tailscale's name setting was changed after setup; LocalPilot leaves it as it is.",
            );
        }
        s.plan(
            &mut journal,
            Attempt {
                step: Step::UndoRename,
                state: State::Planned,
                node_id: owned.node_id.clone(),
                detail: String::new(),
                old_hostname: Some(owned.old_hostname.clone()),
                new_hostname: Some(owned.new_hostname.clone()),
            },
        )?;
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
            let (state, message) = match &answered {
                Ok(reply) if reply.status == 403 => (
                    State::Refused,
                    "Tailscale refused to restore the name; nothing was changed.".to_string(),
                ),
                _ => (
                    State::Unresolved,
                    "whether the old name was restored is not known; LocalPilot keeps its \
                     record. Check the name with `tailscale status`."
                        .to_string(),
                ),
            };
            let _ = s.settle_attempt(&mut journal, state, &message);
            return stop(message);
        }
        journal.rename = None;
        if let Err(error) = s.settle_attempt(&mut journal, State::Done, "") {
            return stop(format!(
                "The old name setting was restored, but LocalPilot could not record it ({error})."
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
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .http1_only()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|error| error.to_string())?;
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

fn system_random(buffer: &mut [u8]) {
    use ring::rand::SecureRandom as _;
    if ring::rand::SystemRandom::new().fill(buffer).is_err() {
        // Without randomness there is no safe name or tag: an all-zero buffer
        // still differs from any real tag by its prefix, and the name is then
        // predictable but harmless.
        buffer.fill(0);
    }
}

/// `~/.localpilot/remote`, beside the device registry.
fn state_dir() -> Result<PathBuf, Stop> {
    crate::server_devices::user_devices_dir()
        .and_then(|devices| devices.parent().map(|home| home.join("remote")))
        .ok_or_else(|| Stop("no home folder to keep setup's record in".to_string()))
}

/// `localpilot remote setup [...]` from the command line.
pub(crate) async fn command(
    check_only: bool,
    undo_run: bool,
    flag: NameFlag,
    yes: bool,
) -> anyhow::Result<()> {
    use std::io::IsTerminal as _;
    let cwd = std::env::current_dir()?;
    let port = localpilot_config::load(
        &localpilot_config::ConfigPaths::standard(&cwd),
        &localpilot_config::CliOverrides::default(),
    )?
    .remote
    .port;
    let console = Terminal;
    if check_only {
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
        random: &system_random,
    };
    let result = if undo_run {
        undo(&setup, yes).await
    } else {
        run(&setup, flag, yes).await
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
        requests: Vec<String>,
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
                requests: Vec::new(),
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
                "Version": "1.104.5-t73a48057a",
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
            if !if_match.starts_with("lp-probe-") {
                if let Some(race) = sim.race.take() {
                    race(&mut sim);
                }
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
        fail_persist: Option<(Step, State)>,
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
            let random = move |buffer: &mut [u8]| {
                let base = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                for (index, byte) in buffer.iter_mut().enumerate() {
                    *byte = base.wrapping_mul(31).wrapping_add(index as u8 * 7);
                }
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
            Some(ServeOwned {
                node_id: "nNODE1CNTRL".into(),
                tailnet: "user@example.com".into(),
                name: "davids-desktop.tail1234.ts.net".into(),
                port: PORT,
                verified: true,
            })
        );
        assert_eq!(journal.rename, None);
        let config = std::fs::read_to_string(world.config_path()).unwrap();
        assert!(config.contains("advertised_url = \"https://davids-desktop.tail1234.ts.net\""));
        assert!(world
            .console
            .text()
            .contains("Verified from this computer; the phone is not checked yet"));

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
            new_name.starts_with("lp-") && new_name.len() == 9,
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
        assert!(world.journal().serve.is_none());
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
            (Ok(200), "accepted a save it should have refused"),
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
            assert!(journal.serve.is_none());
            assert_eq!(journal.last.unwrap().step, Step::Probe);
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
        assert!(journal.serve.is_none());
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
            assert!(journal.serve.is_none(), "nothing is claimed");
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
        assert!(world.journal().serve.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn failing_to_record_an_applied_change_stops_and_says_so() {
        let mut world = World::new();
        world.fail_persist = Some((Step::Serve, State::Done));
        let error = world.go(NameFlag::Keep, true).await.unwrap_err();
        assert!(error.0.contains("could not record it"), "{error}");
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
            assert_eq!((last.step, last.state), (Step::Rename, State::Unresolved));
            assert_eq!(last.old_hostname.as_deref(), Some(""));
            // Nothing is served after it.
            assert_eq!(world.daemon.sim().writes().len(), 2);
            // A later run will not rename over it.
            let again = world.go(NameFlag::Random, true).await.unwrap_err();
            assert!(again.0.contains("no confirmed result"), "{again}");
        }
        let world = World::new();
        world.daemon.sim().patch_answer = Some(Ok(403));
        let error = world.go(NameFlag::Random, true).await.unwrap_err();
        assert!(error.0.contains("nothing was changed"), "{error}");
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
            assert!(!world.journal().serve.unwrap().verified);
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
        assert!(world.journal().serve.is_none());

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
        assert!(world.journal().serve.is_some());

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
            r#"{"version":2}"#,
            r#"{"version":1,"mystery":true}"#,
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

    #[test]
    fn the_config_edit_keeps_everything_else_and_refuses_what_it_cannot_parse() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("config.toml");
        let original = "# my settings\nmodel = \"x\" # keep me\n\n[remote]\nport = 47610\n";
        std::fs::write(&path, original).unwrap();
        assert_eq!(
            edit_config(Some(&path), "https://pc.tail1234.ts.net"),
            Ok(true)
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
            Ok(false)
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
