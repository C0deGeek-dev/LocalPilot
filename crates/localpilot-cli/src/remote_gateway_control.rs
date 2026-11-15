//! Start the remote gateway in the background and stop it again
//! (`localpilot remote gateway start|stop`, ADR-0288).
//!
//! A running gateway writes a control file beside the device registry
//! (owner-only): its port, process id, how it was started and a random stop
//! token. `stop` sends that token to the gateway's loopback-only stop path,
//! which works the same on every platform and never signals a process by id
//! (an id can be reused once the gateway is gone). A browser cannot read the
//! file, so it cannot stop the gateway; a request through the user's proxy
//! carries the proxy's host name and is refused before the token is checked.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::remote_status::GatewayHealth;

/// The loopback-only path a gateway stops on.
pub(crate) const STOP_PATH: &str = "/control/stop";
/// The header that carries the stop token.
pub(crate) const TOKEN_HEADER: &str = "x-localpilot-gateway-token";
/// The control file's name, in the device registry's directory.
const CONTROL_FILE: &str = "gateway.json";
/// How long a background start may take to answer its health check.
const START_WAIT: Duration = Duration::from_secs(10);
/// How long a stop may take before the port is free.
const STOP_WAIT: Duration = Duration::from_secs(10);

/// How a gateway was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StartedBy {
    /// `localpilot remote gateway` in a terminal.
    Foreground,
    /// `localpilot remote gateway start`, or setup after the user agreed.
    Background,
}

/// What a running gateway records about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Control {
    pub(crate) port: u16,
    pub(crate) pid: u32,
    pub(crate) started_by: StartedBy,
    pub(crate) token: String,
}

fn control_dir() -> Option<PathBuf> {
    crate::server_devices::user_devices_dir()
}

/// A fresh stop token: 32 random bytes, hex.
pub(crate) fn new_token() -> anyhow::Result<String> {
    use ring::rand::SecureRandom as _;
    let mut bytes = [0_u8; 32];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("no randomness for the gateway's stop token"))?;
    Ok(bytes
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        }))
}

/// Record a running gateway. Owner-only, replaced atomically.
pub(crate) fn write_control(control: &Control) -> std::io::Result<()> {
    let dir = control_dir()
        .ok_or_else(|| std::io::Error::other("there is no home directory for the control file"))?;
    write_control_in(&dir, control)
}

fn write_control_in(dir: &Path, control: &Control) -> std::io::Result<()> {
    crate::server_devices::prepare_dir(dir)?;
    let bytes = serde_json::to_vec_pretty(control).map_err(std::io::Error::other)?;
    crate::server_devices::replace_restricted(dir, CONTROL_FILE, &bytes)
}

/// The recorded gateway, if a readable control file exists.
pub(crate) fn read_control() -> Option<Control> {
    read_control_in(&control_dir()?)
}

fn read_control_in(dir: &Path) -> Option<Control> {
    let bytes = std::fs::read(dir.join(CONTROL_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Remove the control file, but only while it still describes this gateway
/// (a newer gateway may have replaced it).
pub(crate) fn clear_control(token: &str) {
    if let Some(dir) = control_dir() {
        clear_control_in(&dir, token);
    }
}

fn clear_control_in(dir: &Path, token: &str) {
    if read_control_in(dir).is_some_and(|control| control.token == token) {
        let _ = std::fs::remove_file(dir.join(CONTROL_FILE));
    }
}

/// Whether `presented` is this gateway's stop token (constant time).
pub(crate) fn token_matches(expected: &str, presented: &[u8]) -> bool {
    use subtle::ConstantTimeEq as _;
    expected.len() == presented.len() && bool::from(expected.as_bytes().ct_eq(presented))
}

/// Why the gateway was not started or stopped.
pub(crate) type Refusal = String;

/// `localpilot remote gateway start`: start the gateway in the background
/// unless it already runs. Refuses while remote access is off.
pub(crate) async fn start(cwd: &Path) -> Result<StartOutcome, Refusal> {
    let status = crate::remote_gateway::status_at(cwd).await;
    let settings = status
        .settings
        .map_err(|reason| format!("Remote access settings cannot be read: {reason}"))?;
    if !settings.enabled {
        return Err("Remote access is off, so the gateway is not started. Run `localpilot remote setup` first.".to_string());
    }
    match status.gateway {
        Some(GatewayHealth::Running) => return Ok(StartOutcome::AlreadyRunning),
        Some(GatewayHealth::Unexpected) => {
            return Err(format!(
                "Another service is using the gateway port {}. Check [remote] port.",
                settings.port
            ))
        }
        // Something holds the port without answering: starting another
        // gateway would only fail to bind.
        Some(GatewayHealth::TimedOut) => {
            return Err(
                "Something on the gateway port does not answer; check `localpilot remote status`."
                    .to_string(),
            )
        }
        Some(GatewayHealth::CheckFailed(reason)) => {
            return Err(format!("The gateway port could not be checked: {reason}"))
        }
        Some(GatewayHealth::NotRunning) | None => {}
    }
    let program = std::env::current_exe()
        .map_err(|error| format!("Cannot find this program to start the gateway: {error}"))?;
    localpilot_server::daemon::spawn_detached_argv(
        &program,
        &["remote", "gateway", "--background"],
    )
    .map_err(|error| format!("Could not start the gateway: {error}"))?;
    let deadline = tokio::time::Instant::now() + START_WAIT;
    loop {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if crate::remote_gateway::status_at(cwd).await.gateway == Some(GatewayHealth::Running) {
            return Ok(StartOutcome::Started);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(StartOutcome::NotYet);
        }
    }
}

/// What a start did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartOutcome {
    Started,
    AlreadyRunning,
    /// Started, but it did not answer its health check in time.
    NotYet,
}

/// `localpilot remote gateway stop`: ask the recorded gateway to stop and
/// wait until its port is free.
pub(crate) async fn stop() -> Result<StopOutcome, Refusal> {
    let Some(control) = read_control() else {
        return Ok(StopOutcome::NotRunning);
    };
    match send_stop(&control).await {
        Ok(true) => {}
        Ok(false) => {
            return Err("The gateway refused the stop request; it may be a gateway started by another user or an older version. Stop it where it runs (Ctrl-C).".to_string())
        }
        // Nothing listens: the file is left over from a gateway that ended
        // without removing it.
        Err(_) => {
            clear_control(&control.token);
            return Ok(StopOutcome::NotRunning);
        }
    }
    let deadline = tokio::time::Instant::now() + STOP_WAIT;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if tokio::net::TcpStream::connect(("127.0.0.1", control.port))
            .await
            .is_err()
        {
            return Ok(StopOutcome::Stopped);
        }
    }
    Err("The gateway accepted the stop request but is still running.".to_string())
}

/// What a stop did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopOutcome {
    Stopped,
    NotRunning,
}

/// Send the stop request: `Ok(true)` when the gateway accepted it.
async fn send_stop(control: &Control) -> std::io::Result<bool> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut stream = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect(("127.0.0.1", control.port)),
    )
    .await
    .map_err(|_| std::io::Error::other("timed out"))??;
    let request = format!(
        "POST {STOP_PATH} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{TOKEN_HEADER}: {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        port = control.port,
        token = control.token,
    );
    stream.write_all(request.as_bytes()).await?;
    let mut reply = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut reply)).await;
    Ok(reply.starts_with(b"HTTP/1.1 200"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn the_control_file_is_owner_only_and_cleared_only_by_its_own_gateway() {
        let dir = tempfile::tempdir().unwrap();
        let devices = dir.path().join("devices");
        let control = Control {
            port: 47610,
            pid: 7,
            started_by: StartedBy::Background,
            token: new_token().unwrap(),
        };
        write_control_in(&devices, &control).unwrap();
        assert_eq!(read_control_in(&devices), Some(control.clone()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(devices.join(CONTROL_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "owner-only: {mode:o}");
        }
        clear_control_in(&devices, "another gateway's token");
        assert!(
            read_control_in(&devices).is_some(),
            "a newer gateway's file stays"
        );
        clear_control_in(&devices, &control.token);
        assert!(read_control_in(&devices).is_none());
    }

    #[test]
    fn tokens_are_fresh_and_compared_exactly() {
        let token = new_token().unwrap();
        assert_eq!(token.len(), 64);
        assert_ne!(token, new_token().unwrap());
        assert!(token_matches(&token, token.as_bytes()));
        assert!(!token_matches(&token, &token.as_bytes()[1..]));
        assert!(!token_matches(&token, b""));
    }
}
