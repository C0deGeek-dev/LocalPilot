//! A SearXNG instance on this computer, in a container LocalPilot owns.
//!
//! Everything here is about the container: which runtime runs it and whether
//! that runtime is this computer's own, the pinned image, the settings folder,
//! the record of what LocalPilot created, and the loopback-only liveness and
//! engine checks. The search tool itself is the managed bridge server, set up
//! and started like every other managed backend.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The SearXNG image, pinned by digest; bumped deliberately, together with
/// [IMAGE_TAG] and [GENERAL_ENGINES].
pub(crate) const IMAGE: &str =
    "docker.io/searxng/searxng@sha256:5b72d7ef63a3d214c2a690016ecf53a17f1ab7f02dcc4a1de1669837d9340fc9";
/// The upstream tag of [IMAGE], for people reading the preview.
pub(crate) const IMAGE_TAG: &str = "2026.11.19-368baa340";
/// The general-category engines [IMAGE] enables by default, read from its
/// /config when the pin was bumped (no instance exists before setup to ask).
pub(crate) const GENERAL_ENGINES: &[&str] = &[
    "brave",
    "currency",
    "dictzone",
    "duckduckgo",
    "google cse",
    "lingva",
    "mymemory translated",
];
/// The bridge that offers SearXNG's search as an MCP tool. Pinned.
pub(crate) const BRIDGE_PACKAGE: &str = "mcp-searxng@2.11.0";
/// The bridge's search tool.
pub(crate) const BRIDGE_TOOL: &str = "searxng_web_search";
/// The label every container LocalPilot creates carries.
pub(crate) const LABEL: &str = "dev.localpilot.managed";
pub(crate) const LABEL_VALUE: &str = "search-searxng";
/// The first loopback port tried.
pub(crate) const FIRST_PORT: u16 = 8888;
const PORT_TRIES: u16 = 100;
/// How long setup and a start wait for SearXNG to answer.
pub(crate) const LIVE_WAIT: std::time::Duration = std::time::Duration::from_secs(60);
/// The longest one loopback request may take, connecting and reading.
const PROBE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(3);
/// The oldest Docker Engine on which a port published on 127.0.0.1 cannot be
/// reached from other hosts on the local network.
const MIN_ENGINE: (u32, u32, u32) = (28, 0, 0);
/// The file in the settings folder that marks it as LocalPilot's; it holds the
/// SHA-256 of the settings.yml LocalPilot last wrote.
const MARKER: &str = ".localpilot-searxng";
/// The variables that could point Docker at another daemon; removed from
/// every command bound to the verified endpoint.
pub(crate) const ENDPOINT_VARIABLES: [&str; 4] = [
    "DOCKER_HOST",
    "DOCKER_CONTEXT",
    "DOCKER_TLS_VERIFY",
    "DOCKER_CERT_PATH",
];
/// The Docker network LocalPilot creates for SearXNG. On a user-defined
/// network Docker's own DNS resolves names through the daemon's resolver,
/// which also works where the default bridge cannot reach it (Docker inside
/// WSL, whose resolver is an address only the distribution itself reaches).
pub(crate) const NETWORK: &str = "localpilot-searxng";

/// What a command printed.
#[derive(Debug, Clone, Default)]
pub(crate) struct Ran {
    pub(crate) ok: bool,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: String,
}

impl Ran {
    pub(crate) fn text(&self) -> String {
        decode(&self.stdout).trim().to_string()
    }

    /// The first line of what it printed on stderr, for a message.
    fn why(&self) -> String {
        self.stderr
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("no reason given")
            .to_string()
    }
}

/// Text that may be UTF-16 (what wsl.exe prints) or UTF-8.
fn decode(bytes: &[u8]) -> String {
    let utf16 = bytes.len() >= 2
        && bytes.len() % 2 == 0
        && bytes.iter().skip(1).step_by(2).any(|byte| *byte == 0);
    if utf16 {
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        String::from_utf16_lossy(&units)
            .trim_start_matches('\u{feff}')
            .to_string()
    } else {
        String::from_utf8_lossy(bytes).to_string()
    }
}

/// What the container work touches on this computer, injected so tests use
/// fakes.
pub(crate) trait Host {
    /// Run `argv`. `bound`: the command names its daemon itself, so the
    /// variables that could choose another one are removed.
    fn run(&self, argv: &[String], bound: bool) -> Ran;
    /// GET `path` from 127.0.0.1:`port`: the status and body. Never anything
    /// but loopback.
    fn get(&self, port: u16, path: &str) -> Option<(u16, String)>;
    /// Whether 127.0.0.1:`port` is free to listen on.
    fn port_free(&self, port: u16) -> bool;
    /// Wait between liveness checks.
    fn pause(&self);
    /// Time on a monotonic clock, for deadlines.
    fn now(&self) -> std::time::Duration;
    fn windows(&self) -> bool;
    /// Start `argv` as a process that lives until the returned guard is
    /// dropped; used to keep a WSL distribution up. `None` when it could not
    /// start.
    fn hold(&self, argv: &[String]) -> Option<Held> {
        let _ = argv;
        None
    }
    /// The text of the WSL settings file (`%USERPROFILE%\.wslconfig`):
    /// `Ok(None)` when there is none.
    fn wsl_config(&self) -> Result<Option<String>, String> {
        Ok(None)
    }
}

/// A process kept alive for as long as this value lives.
pub(crate) struct Held(#[allow(dead_code)] Box<dyn std::any::Any + Send + Sync>);

impl Held {
    pub(crate) fn new(value: impl std::any::Any + Send + Sync) -> Self {
        Self(Box::new(value))
    }
}

/// What keeps the WSL distribution, and so its Docker and SearXNG, running
/// while LocalPilot works with them: WSL stops an idle distribution about 30
/// seconds after the last command it was given. Empty on every other route.
/// Clones share one process; it ends when the last clone is dropped.
#[derive(Clone, Default)]
pub(crate) struct Keep(Option<std::sync::Arc<Held>>);

impl Keep {
    pub(crate) fn holds(&self) -> bool {
        self.0.is_some()
    }

    #[cfg(test)]
    pub(crate) fn holding(value: impl std::any::Any + Send + Sync) -> Self {
        Self(Some(std::sync::Arc::new(Held::new(value))))
    }
}

impl std::fmt::Debug for Keep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.holds() {
            "Keep(held)"
        } else {
            "Keep(none)"
        })
    }
}

/// Two engines are the same engine whatever keeps their distribution up.
impl PartialEq for Keep {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Keep {}

/// How Docker is reached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Route {
    /// The docker program on PATH.
    Path { program: PathBuf },
    /// docker inside a WSL distribution.
    Wsl { wsl: PathBuf, distro: String },
}

/// A container runtime that is this computer's own and new enough.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Engine {
    pub(crate) route: Route,
    /// The daemon every command is bound to.
    pub(crate) endpoint: String,
    pub(crate) version: String,
    /// Holds the WSL distribution up while this engine is in use.
    pub(crate) keep: Keep,
}

impl Engine {
    /// The full command for docker `args`, bound to the verified endpoint.
    /// Every container command goes through here.
    pub(crate) fn argv(&self, args: &[&str]) -> Vec<String> {
        let mut argv = match &self.route {
            Route::Path { program } => vec![program.display().to_string()],
            Route::Wsl { wsl, distro } => {
                let mut argv = vec![
                    wsl.display().to_string(),
                    "-d".to_string(),
                    distro.clone(),
                    "--exec".to_string(),
                    "env".to_string(),
                ];
                for variable in ENDPOINT_VARIABLES {
                    argv.push("-u".to_string());
                    argv.push(variable.to_string());
                }
                // --exec runs the program itself, not through the distribution's
                // shell, so arguments (Windows paths, templates) arrive as given.
                argv.push("docker".to_string());
                argv
            }
        };
        argv.push("--host".to_string());
        argv.push(self.endpoint.clone());
        argv.extend(args.iter().map(|arg| (*arg).to_string()));
        argv
    }

    fn run(&self, host: &dyn Host, args: &[&str]) -> Ran {
        host.run(&self.argv(args), true)
    }

    /// For people: which Docker this is.
    pub(crate) fn describe(&self) -> String {
        match &self.route {
            Route::Path { .. } => format!("Docker {}", self.version),
            Route::Wsl { distro, .. } => {
                format!("Docker {} in the WSL distribution {distro}", self.version)
            }
        }
    }

    /// The WSL distribution, when Docker runs in one.
    pub(crate) fn distro(&self) -> Option<&str> {
        match &self.route {
            Route::Wsl { distro, .. } => Some(distro),
            Route::Path { .. } => None,
        }
    }
}

/// What container runtime there is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Found {
    /// Usable runtimes; more than one only for several WSL distributions.
    Ready(Vec<Engine>),
    /// None usable, and why, with what to do.
    Unavailable(String),
}

fn install_guidance() -> String {
    "No Docker was found. Install Docker Desktop (https://docs.docker.com/desktop/) or, on Linux, Docker Engine (https://docs.docker.com/engine/install/), then run localpilot search setup again. LocalPilot never installs it.".to_string()
}

/// Find the container runtime: Docker on PATH first, then (on Windows)
/// Docker inside a WSL distribution under WSL's NAT networking. Podman is
/// reported and not used.
pub(crate) fn detect(host: &dyn Host, find: &dyn Fn(&str) -> Option<PathBuf>) -> Found {
    if let Some(docker) = find("docker") {
        return match resolve(
            host,
            Route::Path {
                program: docker.clone(),
            },
            vec![docker.display().to_string()],
            Purpose::Detect,
        ) {
            Ok(engine) => Found::Ready(vec![engine]),
            Err(reason) => Found::Unavailable(reason),
        };
    }
    if find("podman").is_some() {
        return Found::Unavailable(
            "Podman was found, but SearXNG on Podman is not enabled yet (its localhost-only publishing is still being checked). Install Docker, or choose another backend.".to_string(),
        );
    }
    if host.windows() {
        if let Some(wsl) = find("wsl") {
            return wsl_engines(host, &wsl);
        }
    }
    Found::Unavailable(install_guidance())
}

/// Docker in each WSL distribution where it works.
fn wsl_engines(host: &dyn Host, wsl: &Path) -> Found {
    let listed = host.run(
        &[
            wsl.display().to_string(),
            "-l".to_string(),
            "-q".to_string(),
        ],
        false,
    );
    let mut engines = Vec::new();
    let mut reasons = Vec::new();
    for distro in decode(&listed.stdout)
        .lines()
        .map(|line| line.trim().trim_matches('\0').trim())
        .filter(|line| !line.is_empty() && !line.starts_with("docker-desktop"))
    {
        let prefix = vec![
            wsl.display().to_string(),
            "-d".to_string(),
            distro.to_string(),
            "--exec".to_string(),
            "docker".to_string(),
        ];
        let route = Route::Wsl {
            wsl: wsl.to_path_buf(),
            distro: distro.to_string(),
        };
        match resolve(host, route, prefix, Purpose::Detect) {
            Ok(engine) => engines.push(engine),
            Err(reason) => reasons.push(format!("{distro}: {reason}")),
        }
    }
    if engines.is_empty() {
        let mut message = install_guidance();
        if !reasons.is_empty() {
            message = format!(
                "No usable Docker in WSL ({}). {message}",
                reasons.join("; ")
            );
        }
        Found::Unavailable(message)
    } else {
        Found::Ready(engines)
    }
}

/// What an engine is resolved for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Purpose {
    /// Listing what setup could use: Docker inside WSL must be under WSL's NAT
    /// networking, but nothing is kept running for a choice not yet made.
    Detect,
    /// Creating, starting or checking SearXNG: Docker inside WSL must be under
    /// WSL's NAT networking, and its distribution is kept up while the engine
    /// is in use (no holder, no launch).
    Launch,
    /// Stopping or removing what LocalPilot created: allowed whatever WSL's
    /// networking is now, so cleanup is never blocked by the check that
    /// protects starting.
    Cleanup,
}

/// SearXNG is published only on 127.0.0.1 inside the distribution. Under
/// WSL's NAT networking (its default) WSL forwards that to 127.0.0.1 on
/// Windows only; other modes are not checked, so they are refused. The mode
/// is read from WSL itself, because a changed .wslconfig takes effect only
/// after WSL restarts and a NAT request can fall back to another mode.
fn wsl_networking(host: &dyn Host, wsl: &Path, distro: &str) -> Result<(), String> {
    const HINT: &str = "To use SearXNG, install Docker Desktop (https://docs.docker.com/desktop/), or use WSL's default NAT networking, then run localpilot search setup again.";
    let config = host.wsl_config().map_err(|_| {
        format!("WSL's settings file (.wslconfig) could not be read, so it is not known whether SearXNG's port would reach Windows. {HINT}")
    })?;
    if config.as_deref().is_some_and(forwarding_off) {
        return Err(format!(
            "localhostForwarding is off in .wslconfig, so SearXNG's port inside WSL would not reach Windows. {HINT}"
        ));
    }
    let ran = host.run(
        &[
            wsl.display().to_string(),
            "-d".to_string(),
            distro.to_string(),
            "--exec".to_string(),
            "wslinfo".to_string(),
            "--networking-mode".to_string(),
        ],
        false,
    );
    let mode: String = ran
        .text()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(24)
        .collect();
    if !ran.ok || mode.is_empty() {
        return Err(format!(
            "WSL's networking mode in {distro} could not be read, so it is not known whether SearXNG's port would stay on this computer. {HINT}"
        ));
    }
    if mode != "nat" {
        return Err(format!(
            "WSL in {distro} uses {mode} networking; SearXNG inside WSL is set up only under WSL's default NAT networking, where its port stays on this computer. {HINT}"
        ));
    }
    Ok(())
}

/// Whether .wslconfig turns localhost forwarding off (`[wsl2]`
/// `localhostForwarding = false`).
fn forwarding_off(text: &str) -> bool {
    let mut section = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            section = name.trim().to_ascii_lowercase();
            continue;
        }
        if section != "wsl2" {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let value = value.split(['#', ';']).next().unwrap_or_default();
            if key.trim().eq_ignore_ascii_case("localhostforwarding")
                && value.trim().trim_matches('"').eq_ignore_ascii_case("false")
            {
                return true;
            }
        }
    }
    false
}

/// A process that only waits, run in the distribution to keep it up. It reads
/// the stdin LocalPilot holds open, so it also ends when LocalPilot ends
/// without cleaning up (a crash or a forced kill): the pipe closes and it sees
/// the end of its input.
pub(crate) fn keep_alive_argv(wsl: &Path, distro: &str) -> Vec<String> {
    vec![
        wsl.display().to_string(),
        "-d".to_string(),
        distro.to_string(),
        "--exec".to_string(),
        "cat".to_string(),
    ]
}

/// What keeps `route`'s WSL distribution up; empty on any other route. An
/// error when the holder cannot start: WSL would stop SearXNG mid-use.
fn keep_up(host: &dyn Host, route: &Route) -> Result<Keep, String> {
    let Route::Wsl { wsl, distro } = route else {
        return Ok(Keep::default());
    };
    match host.hold(&keep_alive_argv(wsl, distro)) {
        Some(held) => Ok(Keep(Some(std::sync::Arc::new(held)))),
        None => Err(format!(
            "LocalPilot could not keep the WSL distribution {distro} running (starting {} failed), so WSL would stop SearXNG while it is in use",
            wsl.display()
        )),
    }
}

/// The endpoint Docker would use, as the Docker CLI itself resolves it
/// (DOCKER_HOST, DOCKER_CONTEXT, the current context), accepted only when it
/// is this computer's own daemon and new enough.
fn resolve(
    host: &dyn Host,
    route: Route,
    prefix: Vec<String>,
    purpose: Purpose,
) -> Result<Engine, String> {
    // For a launch the distribution is held first, so everything checked
    // below is checked on the distribution that stays up.
    let keep = match purpose {
        Purpose::Launch => keep_up(host, &route)?,
        Purpose::Detect | Purpose::Cleanup => Keep::default(),
    };
    if let (Route::Wsl { wsl, distro }, Purpose::Detect | Purpose::Launch) = (&route, purpose) {
        wsl_networking(host, wsl, distro)?;
    }
    let mut inspect = prefix;
    inspect.extend(
        [
            "context",
            "inspect",
            "--format",
            "{{json .Endpoints.docker}}",
        ]
        .iter()
        .map(|arg| (*arg).to_string()),
    );
    let ran = host.run(&inspect, false);
    if !ran.ok {
        return Err(format!("Docker does not answer ({})", ran.why()));
    }
    #[derive(Deserialize)]
    struct Endpoint {
        #[serde(rename = "Host")]
        host: String,
        #[serde(rename = "SkipTLSVerify", default)]
        skip_tls_verify: bool,
    }
    let endpoint: Endpoint = serde_json::from_str(&ran.text())
        .map_err(|_| "Docker's endpoint could not be read".to_string())?;
    if endpoint.skip_tls_verify || !local_endpoint(&endpoint.host) {
        return Err(format!(
            "Docker is set to use {}, which is not this computer's own Docker; SearXNG is set up only on a local Docker",
            endpoint.host
        ));
    }
    let mut engine = Engine {
        route,
        endpoint: endpoint.host,
        version: String::new(),
        keep,
    };
    let version = engine.run(host, &["version", "--format", "{{.Server.Version}}"]);
    if !version.ok {
        return Err(format!(
            "the Docker daemon does not answer; is it running? ({})",
            version.why()
        ));
    }
    engine.version = version.text();
    match new_enough(&engine.version, MIN_ENGINE) {
        Some(true) => Ok(engine),
        Some(false) => Err(format!(
            "Docker Engine {} is older than 28.0.0, on which a port published on localhost can be reached from the local network; update Docker",
            engine.version
        )),
        None => Err(format!(
            "Docker's version could not be read ({}); SearXNG needs Docker Engine 28.0.0 or later",
            engine.version
        )),
    }
}

/// The engine on `route` as it resolves now, checked like at setup.
pub(crate) fn resolve_route(
    host: &dyn Host,
    route: &Route,
    purpose: Purpose,
) -> Result<Engine, String> {
    let prefix = match route {
        Route::Path { program } => vec![program.display().to_string()],
        Route::Wsl { wsl, distro } => vec![
            wsl.display().to_string(),
            "-d".to_string(),
            distro.clone(),
            "--exec".to_string(),
            "docker".to_string(),
        ],
    };
    resolve(host, route.clone(), prefix, purpose)
}

/// The recorded instance's engine, refused when Docker now points anywhere
/// but the recorded endpoint.
pub(crate) fn recorded_engine(
    host: &dyn Host,
    owned: &Owned,
    purpose: Purpose,
) -> Result<Engine, String> {
    let engine = resolve_route(host, &owned.route, purpose)?;
    if engine.endpoint != owned.endpoint {
        return Err(format!(
            "Docker now uses {}, not {} where SearXNG was set up; LocalPilot does not touch SearXNG there",
            engine.endpoint, owned.endpoint
        ));
    }
    Ok(engine)
}

/// The recorded container, checked: the same ID, with LocalPilot's label.
pub(crate) fn owned_container(
    host: &dyn Host,
    engine: &Engine,
    owned: &Owned,
) -> Result<Seen, String> {
    let seen = inspect(host, engine, &owned.container_id).ok_or_else(|| {
        format!(
            "the SearXNG container {} is gone; run localpilot search remove, then localpilot search setup",
            short(&owned.container_id)
        )
    })?;
    if seen.id != owned.container_id || !seen.ours {
        return Err(format!(
            "the container {} is not the one LocalPilot created; LocalPilot does not touch it",
            short(&owned.container_id)
        ));
    }
    Ok(seen)
}

/// Make sure the recorded SearXNG runs and answers on `port` before a search:
/// verified ownership, the start recorded first, then a bounded liveness wait.
/// `audit` appends a setup audit record and says whether it was written.
pub(crate) fn ensure_running(
    host: &dyn Host,
    state_dir: &Path,
    port: u16,
    audit: &dyn Fn(&str, &str) -> bool,
) -> Result<Keep, String> {
    let _lock = lock(state_dir)?;
    let owned = load_owned(state_dir).ok_or_else(|| {
        "SearXNG's record is missing; run localpilot search setup again".to_string()
    })??;
    if owned.unfinished() {
        return Err(
            "an update of SearXNG did not finish; run localpilot search update".to_string(),
        );
    }
    if owned.port != port {
        return Err(format!(
            "the config points at port {port} but SearXNG was set up on port {}; run localpilot search setup again",
            owned.port
        ));
    }
    let engine = recorded_engine(host, &owned, Purpose::Launch)?;
    let seen = owned_container(host, &engine, &owned)?;
    if !seen.running {
        // Docker inside WSL publishes into its own port space: after WSL has
        // stopped, something on Windows can hold the port while Docker would
        // still start, so the Windows side is asked first.
        if !host.port_free(port) {
            return Err(format!(
                "SearXNG could not start: port {port} is taken by something else; run localpilot search update to move it to a free port"
            ));
        }
        if !audit(
            "searxng-start",
            &format!("container={} reason=search", owned.container_id),
        ) {
            return Err(
                "the start of SearXNG could not be recorded, so it was not started".to_string(),
            );
        }
        let started = start(host, &engine, &owned.container_id);
        audit(
            "searxng-start-result",
            &format!(
                "container={} result={}",
                owned.container_id,
                if started == Started::Yes {
                    "started"
                } else {
                    "failed"
                }
            ),
        );
        match started {
            Started::Yes => {}
            Started::PortTaken => {
                return Err(format!(
                    "SearXNG could not start: port {port} is taken by something else; run localpilot search start to choose another"
                ))
            }
            Started::No(reason) => return Err(format!("SearXNG could not start ({reason})")),
        }
    }
    if wait_live(host, port, LIVE_WAIT, &|| false) {
        Ok(engine.keep)
    } else {
        Err(format!(
            "SearXNG did not answer on 127.0.0.1:{port} within {} seconds",
            LIVE_WAIT.as_secs()
        ))
    }
}

/// A daemon socket on this computer: a Unix socket by absolute path, or a
/// named pipe on the local machine.
pub(crate) fn local_endpoint(endpoint: &str) -> bool {
    if let Some(path) = endpoint.strip_prefix("unix://") {
        return path.starts_with('/') && path.len() > 1;
    }
    if let Some(pipe) = endpoint.strip_prefix("npipe://") {
        let pipe = pipe.replace('\\', "/");
        return pipe
            .strip_prefix("//./pipe/")
            .is_some_and(|name| !name.is_empty() && !name.contains('/'));
    }
    false
}

/// Whether a Docker Engine version is at least `min`: `MAJOR.MINOR.PATCH`,
/// then optionally a `-prerelease` (which comes before its release, so it
/// must be above `min`) or `+build`. `None` when it is not such a version.
fn new_enough(version: &str, min: (u32, u32, u32)) -> Option<bool> {
    let version = version.trim();
    let end = version.find(['-', '+']).unwrap_or(version.len());
    let (core, rest) = version.split_at(end);
    let numbers: Vec<u32> = core
        .split('.')
        .map(|part| {
            (!part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
                .then(|| part.parse().ok())
                .flatten()
        })
        .collect::<Option<_>>()?;
    let [major, minor, patch] = numbers[..] else {
        return None;
    };
    let found = (major, minor, patch);
    if rest.starts_with('-') {
        Some(found > min)
    } else {
        Some(found >= min)
    }
}

/// The container's name: the image digest's first twelve hex digits and the
/// port, so a staged update's container never shares the running one's name.
pub(crate) fn container_name(port: u16) -> String {
    let digest = IMAGE.rsplit(':').next().unwrap_or_default();
    format!(
        "localpilot-searxng-{}-{port}",
        &digest[..digest.len().min(12)]
    )
}

/// `docker create` for the pinned image, on LocalPilot's own network
/// (`network`: its ID when creating, its name in a preview), published on
/// loopback only.
pub(crate) fn create_args(port: u16, mount: &str, network: &str) -> Vec<String> {
    vec![
        "create".to_string(),
        "--name".to_string(),
        container_name(port),
        "--label".to_string(),
        format!("{LABEL}={LABEL_VALUE}"),
        "--restart".to_string(),
        "no".to_string(),
        "--network".to_string(),
        network.to_string(),
        "-p".to_string(),
        format!("127.0.0.1:{port}:8080"),
        "-v".to_string(),
        format!("{mount}:/etc/searxng/"),
        IMAGE.to_string(),
    ]
}

/// The first free loopback port from `from`.
pub(crate) fn free_port(host: &dyn Host, from: u16) -> Option<u16> {
    (from..from.saturating_add(PORT_TRIES)).find(|port| host.port_free(*port))
}

/// `~/.localpilot/search/searxng`: the settings folder and the ownership record.
pub(crate) fn home(state_dir: &Path) -> PathBuf {
    state_dir.join("searxng")
}

pub(crate) fn settings_dir(state_dir: &Path) -> PathBuf {
    home(state_dir).join("settings")
}

fn owned_path(state_dir: &Path) -> PathBuf {
    home(state_dir).join("state.json")
}

/// The settings LocalPilot writes: JSON results on (the bridge needs them), a
/// random secret, rate limiter and public-instance mode off.
pub(crate) fn settings_yml(secret: &str) -> String {
    format!(
        "# Written by LocalPilot for its local SearXNG. If you edit this file,\n\
         # setup asks before it replaces it.\n\
         use_default_settings: true\n\
         server:\n  secret_key: \"{secret}\"\n  limiter: false\n  public_instance: false\n  image_proxy: false\n\
         search:\n  formats:\n    - html\n    - json\n"
    )
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// What preparing the settings folder did, so a rollback undoes only that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Prepared {
    /// The folder was created by this run.
    pub(crate) made_dir: bool,
}

/// Put the settings in place. A folder LocalPilot did not make is refused; a
/// settings.yml changed since LocalPilot wrote it is replaced only when
/// `replace_edited` says so.
pub(crate) fn prepare_settings(
    dir: &Path,
    secret: &str,
    replace_edited: &dyn Fn() -> bool,
) -> Result<Prepared, String> {
    let marker = dir.join(MARKER);
    let settings = dir.join("settings.yml");
    let made_dir = !dir.exists();
    if made_dir {
        std::fs::create_dir_all(dir)
            .map_err(|error| format!("{} could not be created ({error})", dir.display()))?;
    } else if !marker.exists() {
        let empty = std::fs::read_dir(dir)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false);
        if !empty {
            return Err(format!(
                "{} already exists and was not made by LocalPilot; move it away and run setup again",
                dir.display()
            ));
        }
    } else if let Ok(current) = std::fs::read(&settings) {
        let written = std::fs::read_to_string(&marker).unwrap_or_default();
        if sha256_hex(&current) != written.trim() && !replace_edited() {
            return Ok(Prepared { made_dir });
        }
    }
    let text = settings_yml(secret);
    let write = || -> std::io::Result<()> {
        std::fs::write(&settings, &text)?;
        std::fs::write(&marker, format!("{}\n", sha256_hex(text.as_bytes())))
    };
    write().map_err(|error| format!("the settings could not be written ({error})"))?;
    Ok(Prepared { made_dir })
}

/// Remove the settings folder, only when it carries LocalPilot's marker.
pub(crate) fn remove_settings(dir: &Path) -> Result<(), String> {
    if !dir.exists() {
        return Ok(());
    }
    if !dir.join(MARKER).exists() {
        return Err(format!(
            "{} is not LocalPilot's; it was left",
            dir.display()
        ));
    }
    std::fs::remove_dir_all(dir)
        .map_err(|error| format!("{} could not be removed ({error})", dir.display()))
}

/// Whether a settings folder carries LocalPilot's marker.
pub(crate) fn settings_ours(dir: &Path) -> bool {
    dir.join(MARKER).exists()
}

/// Copy LocalPilot's settings folder into a new folder, so a staged container
/// never shares (or changes) the running one's. A partial copy is removed.
pub(crate) fn copy_settings(from: &Path, to: &Path) -> Result<(), String> {
    if !from.join(MARKER).exists() {
        return Err(format!(
            "{} is not LocalPilot's settings folder",
            from.display()
        ));
    }
    std::fs::create_dir(to)
        .map_err(|error| format!("{} could not be created ({error})", to.display()))?;
    if let Err(error) = copy_tree(from, to) {
        let _ = std::fs::remove_dir_all(to);
        return Err(format!(
            "the settings could not be copied to {} ({error})",
            to.display()
        ));
    }
    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            std::fs::create_dir(&target)?;
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// The path Docker mounts the settings folder from: as is, or translated into
/// the WSL distribution.
pub(crate) fn mount_path(host: &dyn Host, engine: &Engine, dir: &Path) -> Result<String, String> {
    match &engine.route {
        Route::Path { .. } => Ok(dir.display().to_string()),
        Route::Wsl { wsl, distro } => {
            // A folder not made yet is found through its parent.
            let (known, name) = match (dir.exists(), dir.parent(), dir.file_name()) {
                (false, Some(parent), Some(name)) => (parent, Some(name.to_string_lossy())),
                _ => (dir, None),
            };
            let ran = host.run(
                &[
                    wsl.display().to_string(),
                    "-d".to_string(),
                    distro.clone(),
                    "--exec".to_string(),
                    "wslpath".to_string(),
                    "-a".to_string(),
                    known.display().to_string(),
                ],
                false,
            );
            let path = ran.text();
            if ran.ok && path.starts_with('/') {
                Ok(match name {
                    Some(name) => format!("{}/{name}", path.trim_end_matches('/')),
                    None => path,
                })
            } else {
                Err(format!(
                    "the settings folder could not be found from WSL ({})",
                    ran.why()
                ))
            }
        }
    }
}

/// What LocalPilot created and owns, recorded when the container is created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Owned {
    pub(crate) route: Route,
    pub(crate) endpoint: String,
    pub(crate) container_id: String,
    pub(crate) container_name: String,
    pub(crate) image: String,
    pub(crate) port: u16,
    pub(crate) settings_dir: PathBuf,
    /// A newer container an update started and has not yet switched to.
    /// The `[search.managed.searxng]` table exactly as LocalPilot wrote it;
    /// the config is changed only while it still reads exactly so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) entry: Option<localpilot_config::ManagedSearchBackend>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pending: Option<Staged>,
    /// An older container an update switched away from and has not yet
    /// removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) retired: Option<Staged>,
}

/// A container in the middle of an update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Staged {
    pub(crate) container_id: String,
    pub(crate) container_name: String,
    pub(crate) image: String,
    pub(crate) port: u16,
    /// Its own copy of the settings folder.
    pub(crate) settings_dir: PathBuf,
    /// The table that points the bridge at it.
    pub(crate) entry: Option<localpilot_config::ManagedSearchBackend>,
}

impl Owned {
    /// The update leftovers, as people see them.
    pub(crate) fn unfinished(&self) -> bool {
        self.pending.is_some() || self.retired.is_some()
    }
}

/// Held while setup or a lifecycle command changes SearXNG, so two of them
/// never work on the same container, settings or record.
#[derive(Debug)]
pub(crate) struct Lock(#[allow(dead_code)] std::fs::File);

/// Take the SearXNG lock, or say who holds it.
pub(crate) fn lock(state_dir: &Path) -> Result<Lock, String> {
    let dir = home(state_dir);
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("{} could not be created ({error})", dir.display()))?;
    let path = dir.join("searxng.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|error| format!("{} could not be opened ({error})", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Lock(file)),
        Err(std::fs::TryLockError::WouldBlock) => Err(
            "another SearXNG setup or localpilot search command is running; try again when it has finished".to_string(),
        ),
        Err(std::fs::TryLockError::Error(error)) => {
            Err(format!("{} could not be locked ({error})", path.display()))
        }
    }
}

/// The ownership record, if there is one.
pub(crate) fn load_owned(state_dir: &Path) -> Option<Result<Owned, String>> {
    let text = std::fs::read_to_string(owned_path(state_dir)).ok()?;
    Some(
        serde_json::from_str(&text)
            .map_err(|error| format!("the SearXNG record could not be read ({error})")),
    )
}

/// Replace the ownership record in one step.
pub(crate) fn save_owned(state_dir: &Path, owned: &Owned) -> Result<(), String> {
    use std::io::Write as _;
    let dir = home(state_dir);
    let fail =
        |error: &dyn std::fmt::Display| format!("the SearXNG record could not be saved ({error})");
    std::fs::create_dir_all(&dir).map_err(|error| fail(&error))?;
    let text = serde_json::to_string_pretty(owned).map_err(|error| fail(&error))?;
    let mut staged = tempfile::NamedTempFile::new_in(&dir).map_err(|error| fail(&error))?;
    staged
        .write_all(text.as_bytes())
        .map_err(|error| fail(&error))?;
    staged.as_file().sync_all().map_err(|error| fail(&error))?;
    staged
        .persist(owned_path(state_dir))
        .map_err(|error| fail(&error.error))?;
    Ok(())
}

pub(crate) fn forget_owned(state_dir: &Path) -> Result<(), String> {
    match std::fs::remove_file(owned_path(state_dir)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("the SearXNG record could not be removed ({error})"))
        }
        _ => Ok(()),
    }
}

/// A container as Docker reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Seen {
    pub(crate) id: String,
    pub(crate) ours: bool,
    pub(crate) running: bool,
}

/// Look up a container by ID or name.
pub(crate) fn inspect(host: &dyn Host, engine: &Engine, which: &str) -> Option<Seen> {
    let ran = engine.run(
        host,
        &[
            "container",
            "inspect",
            "--format",
            "{{json .Id}} {{json .Config.Labels}} {{json .State.Running}}",
            which,
        ],
    );
    if !ran.ok {
        return None;
    }
    let text = ran.text();
    let mut parts = serde_json::Deserializer::from_str(&text).into_iter::<serde_json::Value>();
    let id = parts.next()?.ok()?.as_str()?.to_string();
    let labels = parts.next()?.ok()?;
    let running = parts.next()?.ok()?.as_bool().unwrap_or(false);
    let ours = labels.get(LABEL).and_then(serde_json::Value::as_str) == Some(LABEL_VALUE);
    Some(Seen { id, ours, running })
}

/// Remove the container `id`, only when Docker reports that exact ID with
/// LocalPilot's label. A container that is already gone is fine.
pub(crate) fn remove_container(host: &dyn Host, engine: &Engine, id: &str) -> Result<(), String> {
    let Some(seen) = inspect(host, engine, id) else {
        return Ok(());
    };
    if seen.id != id || !seen.ours {
        return Err(format!(
            "the container {} is not the one LocalPilot created; it was left",
            short(id)
        ));
    }
    let ran = engine.run(host, &["rm", "--force", id]);
    if ran.ok {
        Ok(())
    } else {
        Err(format!(
            "the container {} could not be removed ({})",
            short(id),
            ran.why()
        ))
    }
}

/// The network named `NETWORK` as Docker reports it now; `None` when there
/// is no such network.
struct Network {
    id: String,
    label: String,
    containers: String,
}

fn network_seen(host: &dyn Host, engine: &Engine) -> Option<Network> {
    let format = format!("{{{{.Id}}}} {{{{index .Labels \"{LABEL}\"}}}} {{{{len .Containers}}}}");
    let ran = engine.run(host, &["network", "inspect", "--format", &format, NETWORK]);
    if !ran.ok {
        return None;
    }
    let text = ran.text();
    let mut words = text.split_whitespace();
    let id = words.next()?.to_string();
    let mut rest: Vec<&str> = words.collect();
    let containers = rest.pop().unwrap_or_default().to_string();
    Some(Network {
        id,
        label: rest.join(" "),
        containers,
    })
}

fn network_id(text: &str) -> Option<String> {
    let id = text.trim();
    (id.len() >= 12 && id.chars().all(|c| c.is_ascii_hexdigit())).then(|| id.to_string())
}

/// LocalPilot's network, created when it is not there; its ID, which is what
/// the container is attached to, so a network swapped in under the same name
/// afterwards is never used. A network of that name LocalPilot did not create
/// is never used.
/// Whether this call created it comes back too, so a failed setup removes
/// only a network it made.
pub(crate) fn ensure_network(host: &dyn Host, engine: &Engine) -> Result<(String, bool), String> {
    match network_seen(host, engine) {
        Some(seen) if seen.label == LABEL_VALUE => network_id(&seen.id)
            .map(|id| (id, false))
            .ok_or_else(|| "Docker did not report the network's ID".to_string()),
        Some(_) => Err(format!(
            "a Docker network named {NETWORK} exists that LocalPilot did not create; setup never uses it (remove or rename it, then run setup again)"
        )),
        None => {
            let label = format!("{LABEL}={LABEL_VALUE}");
            let ran = engine.run(host, &["network", "create", "--label", &label, NETWORK]);
            if ran.ok {
                network_id(&ran.text())
                    .map(|id| (id, true))
                    .ok_or_else(|| "Docker did not report the new network's ID".to_string())
            } else {
                Err(format!(
                    "the Docker network {NETWORK} could not be created ({})",
                    ran.why()
                ))
            }
        }
    }
}

/// Remove LocalPilot's network when it is LocalPilot's and nothing uses it.
/// Whether it is gone (or was never there).
pub(crate) fn remove_network(host: &dyn Host, engine: &Engine) -> bool {
    match network_seen(host, engine) {
        None => true,
        // Removed by the ID that was checked, never by name again.
        Some(seen) if seen.label == LABEL_VALUE && seen.containers == "0" => {
            match network_id(&seen.id) {
                Some(id) => remove_network_id(host, engine, &id),
                None => false,
            }
        }
        Some(_) => false,
    }
}

/// What `create` made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Created {
    /// The container's ID.
    pub(crate) id: String,
    /// The network's ID when this call created the network.
    pub(crate) network: Option<String>,
}

/// Create the container on LocalPilot's network (made first when missing).
/// A network made here is removed again when the container cannot be made.
pub(crate) fn create(
    host: &dyn Host,
    engine: &Engine,
    port: u16,
    mount: &str,
) -> Result<Created, String> {
    let (network, made) = ensure_network(host, engine)?;
    let args = create_args(port, mount, &network);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let ran = engine.run(host, &args);
    let id = ran.text();
    let failed = if !ran.ok {
        Some(format!(
            "the container could not be created ({})",
            ran.why()
        ))
    } else if id.len() < 12 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
        Some("Docker did not report the new container's ID".to_string())
    } else {
        None
    };
    if let Some(reason) = failed {
        if made {
            remove_network_id(host, engine, &network);
        }
        return Err(reason);
    }
    Ok(Created {
        id,
        network: made.then_some(network),
    })
}

/// Remove the network with this exact ID; Docker refuses while a container
/// uses it. Whether it is gone.
pub(crate) fn remove_network_id(host: &dyn Host, engine: &Engine, id: &str) -> bool {
    engine.run(host, &["network", "rm", id]).ok
}

/// How starting a container went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Started {
    Yes,
    /// The port was taken.
    PortTaken,
    No(String),
}

pub(crate) fn start(host: &dyn Host, engine: &Engine, id: &str) -> Started {
    let ran = engine.run(host, &["start", id]);
    if ran.ok {
        return Started::Yes;
    }
    let why = ran.stderr.to_ascii_lowercase();
    if why.contains("port is already allocated")
        || why.contains("address already in use")
        || why.contains("bind for")
        || why.contains("only one usage of each socket address")
    {
        Started::PortTaken
    } else {
        Started::No(ran.why())
    }
}

/// Wait up to `limit` for /healthz to answer OK: process liveness only.
pub(crate) fn wait_live(
    host: &dyn Host,
    port: u16,
    limit: std::time::Duration,
    stopped: &dyn Fn() -> bool,
) -> bool {
    let deadline = host.now() + limit;
    while host.now() < deadline {
        if stopped() {
            return false;
        }
        if let Some((200, body)) = host.get(port, "/healthz") {
            if body.trim() == "OK" {
                return true;
            }
        }
        host.pause();
    }
    false
}

/// The general-category engines the running instance has enabled.
pub(crate) fn live_engines(host: &dyn Host, port: u16) -> Option<Vec<String>> {
    let (200, body) = host.get(port, "/config")? else {
        return None;
    };
    let config: serde_json::Value = serde_json::from_str(&body).ok()?;
    let mut names: Vec<String> = config
        .get("engines")?
        .as_array()?
        .iter()
        .filter(|engine| engine.get("enabled").and_then(serde_json::Value::as_bool) == Some(true))
        .filter(|engine| {
            engine
                .get("categories")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|categories| categories.iter().any(|c| c.as_str() == Some("general")))
        })
        .filter_map(|engine| engine.get("name")?.as_str().map(str::to_string))
        .collect();
    names.sort();
    Some(names)
}

/// A container ID as people see it.
pub(crate) fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}

/// The command line as it will run, for the preview.
pub(crate) fn shown(argv: &[String]) -> String {
    argv.iter()
        .map(|part| {
            if part.contains([' ', '{', '}']) {
                format!("\"{part}\"")
            } else {
                part.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ------------------------------------------------------------- production

/// This computer.
pub(crate) struct LocalHost;

/// [`LocalHost`] with every command given a deadline, for diagnostics that
/// must not hang on a stuck Docker CLI: past it the command is killed and
/// reported as failed.
pub(crate) struct BoundedHost(pub(crate) std::time::Duration);

impl Host for BoundedHost {
    fn run(&self, argv: &[String], bound: bool) -> Ran {
        let Some((program, args)) = argv.split_first() else {
            return Ran::default();
        };
        let mut command = std::process::Command::new(program);
        command
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if bound {
            for variable in ENDPOINT_VARIABLES {
                command.env_remove(variable);
            }
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return Ran {
                    ok: false,
                    stdout: Vec::new(),
                    stderr: error.to_string(),
                }
            }
        };
        let stdout = drain(child.stdout.take());
        let stderr = drain(child.stderr.take());
        let deadline = std::time::Instant::now() + self.0;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    // A command that exits while something it started still
                    // holds its output is not waited for past the deadline.
                    let left = || deadline.saturating_duration_since(std::time::Instant::now());
                    let (Ok(out), Ok(err)) =
                        (stdout.recv_timeout(left()), stderr.recv_timeout(left()))
                    else {
                        return Ran {
                            ok: false,
                            stdout: Vec::new(),
                            stderr: format!(
                                "its output did not close within {} seconds",
                                self.0.as_secs()
                            ),
                        };
                    };
                    return Ran {
                        ok: status.success(),
                        stdout: out,
                        stderr: decode(&err),
                    };
                }
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    // The readers finish on their own: a grandchild may still
                    // hold the pipes.
                    return Ran {
                        ok: false,
                        stdout: Vec::new(),
                        stderr: format!("it did not answer within {} seconds", self.0.as_secs()),
                    };
                }
            }
        }
    }

    fn get(&self, port: u16, path: &str) -> Option<(u16, String)> {
        LocalHost.get(port, path)
    }

    fn port_free(&self, port: u16) -> bool {
        LocalHost.port_free(port)
    }

    fn pause(&self) {
        LocalHost.pause();
    }

    fn now(&self) -> std::time::Duration {
        LocalHost.now()
    }

    fn windows(&self) -> bool {
        LocalHost.windows()
    }
}

/// Read a child's pipe to the end on its own thread; the bytes arrive on the
/// returned channel once the pipe closes.
fn drain<R: std::io::Read + Send + 'static>(pipe: Option<R>) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buffer);
        }
        let _ = sender.send(buffer);
    });
    receiver
}

impl Host for LocalHost {
    fn run(&self, argv: &[String], bound: bool) -> Ran {
        let Some((program, args)) = argv.split_first() else {
            return Ran::default();
        };
        let mut command = std::process::Command::new(program);
        command.args(args).stdin(std::process::Stdio::null());
        if bound {
            for variable in ENDPOINT_VARIABLES {
                command.env_remove(variable);
            }
        }
        match command.output() {
            Ok(output) => Ran {
                ok: output.status.success(),
                stdout: output.stdout,
                stderr: decode(&output.stderr),
            },
            Err(error) => Ran {
                ok: false,
                stdout: Vec::new(),
                stderr: error.to_string(),
            },
        }
    }

    fn get(&self, port: u16, path: &str) -> Option<(u16, String)> {
        use std::io::{Read as _, Write as _};
        let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let deadline = std::time::Instant::now() + PROBE_DEADLINE;
        let left = || {
            deadline
                .checked_duration_since(std::time::Instant::now())
                .filter(|left| !left.is_zero())
        };
        let mut stream = std::net::TcpStream::connect_timeout(&address, left()?).ok()?;
        stream.set_write_timeout(left()).ok()?;
        write!(
            stream,
            "GET {path} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nAccept: */*\r\nConnection: close\r\n\r\n"
        )
        .ok()?;
        // A total deadline, not one per read: a slow trickle cannot stretch it.
        let mut response = Vec::new();
        let mut chunk = [0u8; 16 * 1024];
        loop {
            stream.set_read_timeout(Some(left()?)).ok()?;
            match stream.read(&mut chunk).ok()? {
                0 => break,
                read => response.extend_from_slice(&chunk[..read]),
            }
            if response.len() > 8 * 1024 * 1024 {
                return None;
            }
        }
        let text = String::from_utf8_lossy(&response);
        let (head, body) = text.split_once("\r\n\r\n")?;
        let status = head.split_whitespace().nth(1)?.parse().ok()?;
        Some((status, body.to_string()))
    }

    fn port_free(&self, port: u16) -> bool {
        std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
    }

    fn pause(&self) {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    fn now(&self) -> std::time::Duration {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        START.get_or_init(std::time::Instant::now).elapsed()
    }

    fn windows(&self) -> bool {
        cfg!(windows)
    }

    fn hold(&self, argv: &[String]) -> Option<Held> {
        let (program, args) = argv.split_first()?;
        // The child keeps the write end of its stdin: nothing is ever
        // written, and the pipe closes when this process ends however it ends.
        let child = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        Some(Held::new(Stopped(std::sync::Mutex::new(child))))
    }

    fn wsl_config(&self) -> Result<Option<String>, String> {
        let Some(home) = std::env::var_os("USERPROFILE") else {
            return Ok(None);
        };
        match std::fs::read_to_string(PathBuf::from(home).join(".wslconfig")) {
            Ok(text) => Ok(Some(text)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }
}

/// A child process ended when this is dropped.
struct Stopped(std::sync::Mutex<std::process::Child>);

impl Drop for Stopped {
    fn drop(&mut self) {
        if let Ok(child) = self.0.get_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod bounded_tests {
    use super::*;

    #[test]
    fn a_bounded_command_is_stopped_at_its_deadline() {
        let host = BoundedHost(std::time::Duration::from_millis(300));
        let slow: Vec<String> = if cfg!(windows) {
            [
                "powershell",
                "-NoProfile",
                "-Command",
                "Start-Sleep -Seconds 20",
            ]
        } else {
            ["sh", "-c", "sleep 20", ""]
        }
        .iter()
        .filter(|word| !word.is_empty())
        .map(ToString::to_string)
        .collect();
        let started = std::time::Instant::now();
        let ran = host.run(&slow, false);
        assert!(!ran.ok);
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        let quick: Vec<String> = if cfg!(windows) {
            vec!["cmd".to_string(), "/C".to_string(), "echo hi".to_string()]
        } else {
            vec!["sh".to_string(), "-c".to_string(), "echo hi".to_string()]
        };
        let ran = host.run(&quick, false);
        assert!(ran.ok);
        assert!(String::from_utf8_lossy(&ran.stdout).contains("hi"));
        // The command exits at once but leaves a child holding its output for
        // 8 s: the result comes at the deadline, well before the child ends.
        let detached: Vec<String> = if cfg!(windows) {
            vec![
                "cmd".to_string(),
                "/C".to_string(),
                "start /B powershell -NoProfile -Command Start-Sleep -Seconds 8".to_string(),
            ]
        } else {
            vec!["sh".to_string(), "-c".to_string(), "sleep 8 &".to_string()]
        };
        let started = std::time::Instant::now();
        let ran = host.run(&detached, false);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(!ran.ok);
        assert!(ran.stderr.contains("did not close"), "{}", ran.stderr);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A command and whether it was bound to its endpoint.
    type Call = (Vec<String>, bool);
    /// How a fake host answers a command.
    type Reply = Box<dyn Fn(&[String]) -> Ran>;

    /// A scripted container host: `reply` answers each command by its words.
    pub(crate) struct FakeHost {
        pub(crate) calls: RefCell<Vec<Call>>,
        pub(crate) reply: Reply,
        pub(crate) live_after: std::cell::Cell<u32>,
        pub(crate) clock: std::cell::Cell<std::time::Duration>,
        pub(crate) taken: Vec<u16>,
        pub(crate) windows: bool,
        pub(crate) config: Option<String>,
        /// What `wsl_config` returns.
        pub(crate) wslconfig: Result<Option<String>, String>,
        /// The WSL keep-alive cannot be started.
        pub(crate) hold_fails: bool,
    }

    pub(crate) fn ok(text: &str) -> Ran {
        Ran {
            ok: true,
            stdout: text.as_bytes().to_vec(),
            stderr: String::new(),
        }
    }

    pub(crate) fn failed(why: &str) -> Ran {
        Ran {
            ok: false,
            stdout: Vec::new(),
            stderr: why.to_string(),
        }
    }

    impl FakeHost {
        pub(crate) fn new(reply: impl Fn(&[String]) -> Ran + 'static) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                reply: Box::new(reply),
                live_after: std::cell::Cell::new(0),
                clock: std::cell::Cell::new(std::time::Duration::ZERO),
                taken: Vec::new(),
                windows: false,
                wslconfig: Ok(None),
                hold_fails: false,
                config: Some(
                    r#"{"engines":[{"name":"duckduckgo","enabled":true,"categories":["general","web"]},{"name":"wikipedia","enabled":true,"categories":["general"]},{"name":"bing","enabled":false,"categories":["general"]},{"name":"arxiv","enabled":true,"categories":["science"]}]}"#
                        .to_string(),
                ),
            }
        }

        /// The commands run, as text.
        pub(crate) fn commands(&self) -> Vec<String> {
            self.calls
                .borrow()
                .iter()
                .map(|(argv, _)| argv.join(" "))
                .collect()
        }
    }

    impl Host for FakeHost {
        fn run(&self, argv: &[String], bound: bool) -> Ran {
            self.calls.borrow_mut().push((argv.to_vec(), bound));
            (self.reply)(argv)
        }
        fn get(&self, port: u16, path: &str) -> Option<(u16, String)> {
            let _ = port;
            match path {
                "/healthz" => {
                    let left = self.live_after.get();
                    if left == 0 {
                        Some((200, "OK".to_string()))
                    } else {
                        // A probe that gets nothing uses its whole deadline.
                        self.live_after.set(left - 1);
                        self.clock.set(self.clock.get() + PROBE_DEADLINE);
                        None
                    }
                }
                "/config" => self.config.clone().map(|body| (200, body)),
                _ => None,
            }
        }
        fn port_free(&self, port: u16) -> bool {
            !self.taken.contains(&port)
        }
        fn pause(&self) {
            self.clock
                .set(self.clock.get() + std::time::Duration::from_secs(1));
        }
        fn now(&self) -> std::time::Duration {
            self.clock.get()
        }
        fn windows(&self) -> bool {
            self.windows
        }
        fn hold(&self, argv: &[String]) -> Option<Held> {
            let mut held = vec!["(held)".to_string()];
            held.extend(argv.iter().cloned());
            self.calls.borrow_mut().push((held, false));
            (!self.hold_fails).then(|| Held::new(()))
        }
        fn wsl_config(&self) -> Result<Option<String>, String> {
            self.wslconfig.clone()
        }
    }

    /// Words of a command after the binding (`--host <endpoint>`).
    pub(crate) fn after_host(argv: &[String]) -> Vec<&str> {
        argv.iter()
            .position(|word| word == "--host")
            .map(|at| argv[at + 2..].iter().map(String::as_str).collect())
            .unwrap_or_default()
    }

    pub(crate) const ID: &str = "4f1c2b3a4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8";
    pub(crate) const NET_ID: &str =
        "5e1f0a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7";

    /// A local, current Docker that creates, starts and inspects one container.
    pub(crate) fn docker_reply(argv: &[String]) -> Ran {
        let words = after_host(argv);
        if argv.iter().any(|word| word == "context") {
            return ok(r#"{"Host":"unix:///var/run/docker.sock","SkipTLSVerify":false}"#);
        }
        match words.as_slice() {
            ["version", ..] => ok("29.6.0"),
            ["create", ..] => ok(ID),
            ["start", _] => ok(ID),
            ["container", "inspect", _, _, which] if *which == ID => {
                ok(&format!("\"{ID}\" {{\"{LABEL}\":\"{LABEL_VALUE}\"}} true"))
            }
            ["container", "inspect", ..] => failed("Error: No such container"),
            ["rm", "--force", _] => ok(ID),
            ["network", "inspect", ..] => failed("Error: network localpilot-searxng not found"),
            ["network", "create", ..] => ok(NET_ID),
            ["network", "rm", _] => ok(NETWORK),
            _ => failed("unexpected command"),
        }
    }

    fn find_docker(name: &str) -> Option<PathBuf> {
        (name == "docker").then(|| PathBuf::from("/usr/bin/docker"))
    }

    #[test]
    fn only_this_computers_own_daemon_is_accepted() {
        for endpoint in [
            "unix:///var/run/docker.sock",
            "unix:///home/me/.docker/run/docker.sock",
            "npipe:////./pipe/docker_engine",
            "npipe:////./pipe/dockerDesktopLinuxEngine",
        ] {
            assert!(local_endpoint(endpoint), "{endpoint}");
        }
        for endpoint in [
            "tcp://127.0.0.1:2375",
            "tcp://192.168.1.5:2376",
            "ssh://me@box",
            "npipe:////server/pipe/docker_engine",
            "npipe://./pipe/docker_engine",
            "npipe:////./pipe/",
            "unix://var/run/docker.sock",
            "unix://",
            "",
        ] {
            assert!(!local_endpoint(endpoint), "{endpoint}");
        }
    }

    #[test]
    fn the_endpoint_docker_itself_resolves_decides_and_binds_every_command() {
        // Whatever chose it (DOCKER_HOST, DOCKER_CONTEXT, the current context),
        // the endpoint is what the Docker CLI reports for its current context.
        let remote = FakeHost::new(|argv| {
            if argv.iter().any(|word| word == "context") {
                ok(r#"{"Host":"ssh://me@builder","SkipTLSVerify":false}"#)
            } else {
                ok("29.6.0")
            }
        });
        let Found::Unavailable(reason) = detect(&remote, &find_docker) else {
            panic!("a remote context must be refused");
        };
        assert!(reason.contains("ssh://me@builder"), "{reason}");
        // Nothing but the endpoint question went to the remote daemon.
        assert_eq!(remote.calls.borrow().len(), 1);

        let local = FakeHost::new(docker_reply);
        let Found::Ready(engines) = detect(&local, &find_docker) else {
            panic!("a local daemon must be accepted");
        };
        assert_eq!(engines[0].endpoint, "unix:///var/run/docker.sock");
        assert_eq!(engines[0].version, "29.6.0");
        let calls = local.calls.borrow();
        // The endpoint question is asked as Docker sees it (its variables kept);
        // every later command names the endpoint and drops the variables.
        assert!(!calls[0].1);
        for (argv, bound) in calls.iter().skip(1) {
            assert!(*bound, "{argv:?}");
            assert_eq!(
                argv[1..3],
                ["--host", "unix:///var/run/docker.sock"],
                "{argv:?}"
            );
        }
        let created = engines[0].argv(&["create"]);
        assert_eq!(created[1..3], ["--host", "unix:///var/run/docker.sock"]);
    }

    #[test]
    fn an_old_or_silent_engine_is_refused_with_what_to_do() {
        let old = FakeHost::new(|argv| {
            if argv.iter().any(|word| word == "context") {
                ok(r#"{"Host":"unix:///var/run/docker.sock"}"#)
            } else {
                ok("27.5.1")
            }
        });
        let Found::Unavailable(reason) = detect(&old, &find_docker) else {
            panic!("an old engine must be refused");
        };
        assert!(
            reason.contains("27.5.1") && reason.contains("28.0.0"),
            "{reason}"
        );

        let down = FakeHost::new(|argv| {
            if argv.iter().any(|word| word == "context") {
                ok(r#"{"Host":"npipe:////./pipe/docker_engine"}"#)
            } else {
                failed("error during connect: the system cannot find the file specified")
            }
        });
        let Found::Unavailable(reason) = detect(&down, &find_docker) else {
            panic!("a daemon that does not answer must be refused");
        };
        assert!(reason.contains("is it running"), "{reason}");
        for (version, expected) in [
            ("28.0.0", Some(true)),
            ("29.6.0", Some(true)),
            ("29.6.0-rc.1", Some(true)),
            ("28.0.1-beta.2", Some(true)),
            ("28.0.0+azure-1", Some(true)),
            ("28.0.0-rc.1", Some(false)),
            ("27.99.9", Some(false)),
            ("28.0", None),
            ("v28.0.0", None),
            ("Docker 29.6.0", None),
            ("29.6.0.1", None),
            ("", None),
        ] {
            assert_eq!(new_enough(version, MIN_ENGINE), expected, "{version}");
        }
    }

    #[test]
    fn podman_wsl_and_nothing_found_are_reported_not_used() {
        let host = FakeHost::new(|_| failed("must not run"));
        let podman = |name: &str| (name == "podman").then(|| PathBuf::from("/usr/bin/podman"));
        let Found::Unavailable(reason) = detect(&host, &podman) else {
            panic!("Podman is not enabled yet");
        };
        assert!(reason.contains("Podman"), "{reason}");
        let Found::Unavailable(reason) = detect(&host, &|_| None) else {
            panic!("nothing found");
        };
        assert!(
            reason.contains("https://docs.docker.com/desktop/"),
            "{reason}"
        );
        assert!(host.calls.borrow().is_empty());
    }

    #[test]
    fn docker_in_wsl_is_bound_inside_the_distribution() {
        let mut host = FakeHost::new(|argv| {
            if argv.iter().any(|word| word == "-l") {
                // What wsl.exe prints: UTF-16 with a byte-order mark.
                let text = "\u{feff}Ubuntu\r\ndocker-desktop\r\nRemote\r\n";
                return Ran {
                    ok: true,
                    stdout: text.encode_utf16().flat_map(u16::to_le_bytes).collect(),
                    stderr: String::new(),
                };
            }
            let distro = argv.get(2).cloned().unwrap_or_default();
            if argv.iter().any(|word| word == "wslinfo") {
                return ok("nat\n");
            }
            if argv.iter().any(|word| word == "context") {
                return if distro == "Remote" {
                    ok(r#"{"Host":"tcp://10.0.0.2:2375"}"#)
                } else {
                    ok(r#"{"Host":"unix:///var/run/docker.sock"}"#)
                };
            }
            ok("29.6.0")
        });
        host.windows = true;
        let wsl = |name: &str| (name == "wsl").then(|| PathBuf::from("wsl.exe"));
        let Found::Ready(engines) = detect(&host, &wsl) else {
            panic!("Docker in Ubuntu is usable");
        };
        assert_eq!(engines.len(), 1);
        assert_eq!(engines[0].distro(), Some("Ubuntu"));
        let argv = engines[0].argv(&["ps"]);
        assert_eq!(
            argv.join(" "),
            "wsl.exe -d Ubuntu --exec env -u DOCKER_HOST -u DOCKER_CONTEXT -u DOCKER_TLS_VERIFY -u DOCKER_CERT_PATH docker --host unix:///var/run/docker.sock ps"
        );
        // The remote context inside a distribution was asked only where it points.
        let remote: Vec<String> = host
            .commands()
            .into_iter()
            .filter(|command| command.contains(" Remote "))
            .collect();
        assert_eq!(remote.len(), 2, "{remote:?}");
        // The networking mode was read from WSL itself. Listing the choices
        // keeps nothing running; the chosen engine is kept up when used.
        assert!(host
            .commands()
            .contains(&"wsl.exe -d Ubuntu --exec wslinfo --networking-mode".to_string()));
        let held = "(held) wsl.exe -d Ubuntu --exec cat".to_string();
        assert!(!engines[0].keep.holds());
        assert!(!host.commands().contains(&held));
        // Launching the chosen engine holds first, then checks again.
        let before = host.commands().len();
        let chosen = resolve_route(&host, &engines[0].route, Purpose::Launch).unwrap();
        assert!(chosen.keep.holds());
        let after = host.commands()[before..].to_vec();
        assert_eq!(after.first(), Some(&held), "{after:?}");
        assert!(after.iter().any(|c| c.contains("wslinfo")), "{after:?}");
    }

    #[test]
    fn a_wsl_launch_without_its_keep_alive_is_refused() {
        let route = Route::Wsl {
            wsl: PathBuf::from("wsl.exe"),
            distro: "Ubuntu".to_string(),
        };
        let engine = resolve_route(&wsl_host(Some("nat")), &route, Purpose::Launch).unwrap();
        assert!(engine.keep.holds());
        let mut failing = wsl_host(Some("nat"));
        failing.hold_fails = true;
        let error = resolve_route(&failing, &route, Purpose::Launch).unwrap_err();
        assert!(
            error.contains("could not keep the WSL distribution Ubuntu running"),
            "{error}"
        );
        // Cleanup needs no keep-alive and starts none.
        let engine = resolve_route(&failing, &route, Purpose::Cleanup).unwrap();
        assert!(!engine.keep.holds());
        // A local Docker needs none either.
        let mut local = FakeHost::new(docker_reply);
        local.hold_fails = true;
        let docker = Route::Path {
            program: PathBuf::from("docker"),
        };
        assert!(resolve_route(&local, &docker, Purpose::Launch).is_ok());
    }

    /// A WSL host whose one distribution runs Docker, answering wslinfo with
    /// `mode`.
    fn wsl_host(mode: Option<&'static str>) -> FakeHost {
        let mut host = FakeHost::new(move |argv| {
            if argv.iter().any(|word| word == "wslinfo") {
                return mode.map_or_else(|| failed("wslinfo: not found"), ok);
            }
            if argv.iter().any(|word| word == "context") {
                return ok(r#"{"Host":"unix:///var/run/docker.sock"}"#);
            }
            ok("29.6.0")
        });
        host.windows = true;
        host
    }

    #[test]
    fn docker_in_wsl_is_used_only_under_nat_with_forwarding_on() {
        let route = Route::Wsl {
            wsl: PathBuf::from("wsl.exe"),
            distro: "Ubuntu".to_string(),
        };
        let ready = wsl_host(Some("nat"));
        assert!(resolve_route(&ready, &route, Purpose::Launch).is_ok());
        for (host, expected) in [
            (wsl_host(Some("mirrored")), "uses mirrored networking"),
            (wsl_host(Some("virtioproxy")), "uses virtioproxy networking"),
            (wsl_host(None), "could not be read"),
        ] {
            let error = resolve_route(&host, &route, Purpose::Launch).unwrap_err();
            assert!(error.contains(expected), "{error}");
            assert!(error.contains("Docker Desktop"), "{error}");
            // Refused before Docker was asked anything; the holder taken
            // first ends with the refusal.
            assert!(
                !host.commands().iter().any(|c| c.contains("docker")),
                "{:?}",
                host.commands()
            );
        }
        let mut off = wsl_host(Some("nat"));
        off.wslconfig = Ok(Some(
            "[wsl2]\nmemory=8GB\nlocalhostForwarding = False # off\n".to_string(),
        ));
        let error = resolve_route(&off, &route, Purpose::Launch).unwrap_err();
        assert!(error.contains("localhostForwarding is off"), "{error}");
        let mut unreadable = wsl_host(Some("nat"));
        unreadable.wslconfig = Err("access denied".to_string());
        let error = resolve_route(&unreadable, &route, Purpose::Launch).unwrap_err();
        assert!(error.contains("could not be read"), "{error}");
        // Forwarding named in another section, or on, is not "off".
        assert!(!forwarding_off(
            "[experimental]\nlocalhostForwarding=false\n"
        ));
        assert!(!forwarding_off("[wsl2]\nlocalhostForwarding=true\n"));
        assert!(forwarding_off("[WSL2]\nLocalhostForwarding=\"false\"\n"));
        // Cleanup works whatever the networking: no check, nothing held.
        let mirrored = wsl_host(Some("mirrored"));
        let engine = resolve_route(&mirrored, &route, Purpose::Cleanup).unwrap();
        assert!(!engine.keep.holds());
        assert!(!mirrored.commands().iter().any(|c| c.contains("wslinfo")));
    }

    #[test]
    fn the_container_joins_localpilots_own_network() {
        let host = FakeHost::new(docker_reply);
        let Found::Ready(engines) = detect(&host, &find_docker) else {
            panic!("docker");
        };
        let engine = &engines[0];
        assert!(!engine.keep.holds(), "only WSL is held");
        let args = create_args(8888, "/s", NETWORK).join(" ");
        assert!(
            args.contains(&format!("--network {NETWORK} -p 127.0.0.1:8888:8080")),
            "{args}"
        );
        create(&host, engine, 8888, "/s").unwrap();
        let commands = host.commands();
        // Attached by the ID Docker reported, not by name.
        assert!(
            commands
                .iter()
                .any(|c| c.contains(" create --name ")
                    && c.contains(&format!("--network {NET_ID} "))),
            "{commands:?}"
        );
        let made = commands
            .iter()
            .position(|c| {
                c.contains(&format!(
                    "network create --label {LABEL}={LABEL_VALUE} {NETWORK}"
                ))
            })
            .expect("network created");
        let created = commands
            .iter()
            .position(|c| c.contains(" create --name "))
            .unwrap();
        assert!(made < created, "{commands:?}");

        // A network of that name LocalPilot did not create: refused, unused.
        let foreign = FakeHost::new(|argv| match after_host(argv).as_slice() {
            ["network", "inspect", ..] => ok(&format!("{NET_ID} <no value> 3")),
            _ => docker_reply(argv),
        });
        let error = create(&foreign, engine, 8888, "/s").unwrap_err();
        assert!(error.contains("LocalPilot did not create"), "{error}");
        assert!(!foreign
            .commands()
            .iter()
            .any(|c| c.contains(" create --name ")));
        assert!(!remove_network(&foreign, engine));

        // Removed only when it is ours and empty.
        let ours_busy = FakeHost::new(|argv| match after_host(argv).as_slice() {
            ["network", "inspect", ..] => ok(&format!("{NET_ID} {LABEL_VALUE} 1")),
            _ => docker_reply(argv),
        });
        assert!(!remove_network(&ours_busy, engine));
        assert!(!ours_busy
            .commands()
            .iter()
            .any(|c| c.contains("network rm")));
        let ours_empty = FakeHost::new(|argv| match after_host(argv).as_slice() {
            ["network", "inspect", ..] => ok(&format!("{NET_ID} {LABEL_VALUE} 0")),
            _ => docker_reply(argv),
        });
        assert!(remove_network(&ours_empty, engine));
        // Removed by the ID that was checked, never by name again.
        assert!(ours_empty
            .commands()
            .iter()
            .any(|c| c.ends_with(&format!("network rm {NET_ID}"))));
        // An existing network of ours is used by its ID too.
        let ours = FakeHost::new(|argv| match after_host(argv).as_slice() {
            ["network", "inspect", ..] => ok(&format!("{NET_ID} {LABEL_VALUE} 0")),
            _ => docker_reply(argv),
        });
        assert_eq!(
            ensure_network(&ours, engine).unwrap(),
            (NET_ID.to_string(), false)
        );
        assert!(!ours.commands().iter().any(|c| c.contains("network create")));
    }

    #[test]
    fn wsl_output_is_read_as_utf16() {
        let bytes: Vec<u8> = "\u{feff}Ubuntu\r\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(decode(&bytes).trim(), "Ubuntu");
        assert_eq!(decode(b"29.6.0\n").trim(), "29.6.0");
    }

    #[test]
    fn settings_are_written_once_kept_when_edited_and_never_in_a_foreign_folder() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("settings");
        let prepared = prepare_settings(&dir, "s1", &|| panic!("nothing to ask")).unwrap();
        assert!(prepared.made_dir);
        let text = std::fs::read_to_string(dir.join("settings.yml")).unwrap();
        for needed in [
            "use_default_settings: true",
            "secret_key: \"s1\"",
            "limiter: false",
            "public_instance: false",
            "- json",
        ] {
            assert!(text.contains(needed), "{needed}");
        }
        // Unedited: replaced without asking.
        let again = prepare_settings(&dir, "s2", &|| panic!("unedited needs no question")).unwrap();
        assert!(!again.made_dir);
        assert!(std::fs::read_to_string(dir.join("settings.yml"))
            .unwrap()
            .contains("s2"));
        // Edited by hand: kept unless the person agrees.
        std::fs::write(dir.join("settings.yml"), "mine\n").unwrap();
        prepare_settings(&dir, "s3", &|| false).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("settings.yml")).unwrap(),
            "mine\n"
        );
        prepare_settings(&dir, "s4", &|| true).unwrap();
        assert!(std::fs::read_to_string(dir.join("settings.yml"))
            .unwrap()
            .contains("s4"));
        // A folder LocalPilot did not make is refused and left alone.
        let foreign = root.path().join("foreign");
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::write(foreign.join("settings.yml"), "theirs\n").unwrap();
        assert!(prepare_settings(&foreign, "s", &|| true).is_err());
        assert!(remove_settings(&foreign).is_err());
        assert!(foreign.join("settings.yml").exists());
        remove_settings(&dir).unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn only_the_recorded_labelled_container_is_removed() {
        let host = FakeHost::new(docker_reply);
        let Found::Ready(engines) = detect(&host, &find_docker) else {
            panic!("docker");
        };
        let engine = &engines[0];
        remove_container(&host, engine, ID).unwrap();
        assert!(host
            .commands()
            .iter()
            .any(|command| command.ends_with(&format!("rm --force {ID}"))));

        let unlabelled = FakeHost::new(|argv| {
            if after_host(argv).first() == Some(&"container") {
                ok(&format!("\"{ID}\" {{\"other\":\"x\"}} true"))
            } else {
                docker_reply(argv)
            }
        });
        assert!(remove_container(&unlabelled, engine, ID).is_err());
        assert!(!unlabelled
            .commands()
            .iter()
            .any(|command| command.contains(" rm ")));
    }

    #[test]
    fn the_container_is_published_on_loopback_only_with_its_label() {
        let args = create_args(8890, "/home/me/settings", NETWORK).join(" ");
        assert!(args.contains("-p 127.0.0.1:8890:8080"), "{args}");
        assert!(args.contains(&format!("--label {LABEL}={LABEL_VALUE}")));
        assert!(args.contains("--restart no"));
        assert!(args.contains("--name localpilot-searxng-5b72d7ef63a3-8890"));
        assert!(args.ends_with(IMAGE));
    }

    #[test]
    fn the_running_instance_reports_its_general_engines() {
        let host = FakeHost::new(docker_reply);
        assert_eq!(
            live_engines(&host, 8888).unwrap(),
            vec!["duckduckgo".to_string(), "wikipedia".to_string()]
        );
        assert!(wait_live(&host, 8888, LIVE_WAIT, &|| false));
        // Bounded by time, not by attempts: each failed probe uses its whole
        // deadline, so far fewer than sixty probes fit in the minute.
        host.live_after.set(1000);
        let started = host.now();
        assert!(!wait_live(&host, 8888, LIVE_WAIT, &|| false));
        let spent = host.now() - started;
        assert!(
            spent >= LIVE_WAIT && spent < LIVE_WAIT + std::time::Duration::from_secs(5),
            "{spent:?}"
        );
        assert!(1000 - host.live_after.get() < 20);
    }

    #[test]
    fn one_searxng_change_at_a_time() {
        let root = tempfile::tempdir().unwrap();
        let held = lock(root.path()).unwrap();
        assert!(lock(root.path())
            .unwrap_err()
            .contains("another SearXNG setup"));
        drop(held);
        lock(root.path()).unwrap();
    }

    #[test]
    fn the_ownership_record_round_trips() {
        let root = tempfile::tempdir().unwrap();
        let owned = Owned {
            route: Route::Wsl {
                wsl: PathBuf::from("wsl.exe"),
                distro: "Ubuntu".to_string(),
            },
            endpoint: "unix:///var/run/docker.sock".to_string(),
            container_id: ID.to_string(),
            container_name: container_name(8888),
            image: IMAGE.to_string(),
            port: 8888,
            settings_dir: settings_dir(root.path()),
            entry: None,
            pending: None,
            retired: None,
        };
        assert!(load_owned(root.path()).is_none());
        save_owned(root.path(), &owned).unwrap();
        assert_eq!(load_owned(root.path()).unwrap().unwrap(), owned);
        forget_owned(root.path()).unwrap();
        assert!(load_owned(root.path()).is_none());
    }
}
