//! What `localpilot remote status` and `localpilot doctor` report about
//! remote access. Data and rendering only: the gateway module collects it, so
//! this file has no dependencies beyond the standard library and `serde`, and
//! the doctor's standalone test build can include it.

use serde::Serialize;

/// Whether the gateway answers on its loopback port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayHealth {
    Running,
    NotRunning,
    /// Something answers on the port, but not this gateway.
    Unexpected,
}

/// The `[remote]` settings as `remote status` reports them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoteSettings {
    pub enabled: bool,
    /// The advertised origin; `Err` names why the configured URL is invalid.
    pub advertised: Result<Option<String>, String>,
    pub port: u16,
}

/// What `localpilot remote status` shows. No secrets. A part that cannot be
/// read is reported with its reason, never as an empty or default value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoteStatus {
    pub settings: Result<RemoteSettings, String>,
    /// `None` when not checked: the settings (and so the port) are unknown,
    /// or remote access is off.
    pub gateway: Option<GatewayHealth>,
    /// Workspaces served with remote access on: label and discovery id.
    pub workspaces: Vec<(String, String)>,
    /// Paired and revoked device counts.
    pub devices: Result<(usize, usize), String>,
}

/// The body `/healthz` answers with, which `localpilot remote status` checks.
pub fn health_body() -> String {
    format!("localpilot-gateway {}", env!("CARGO_PKG_VERSION"))
}

/// The status as indented text lines.
pub fn render_status(status: &RemoteStatus) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    match &status.settings {
        Ok(settings) => {
            let enabled = if settings.enabled {
                "yes"
            } else {
                "no ([remote] enabled = false)"
            };
            let _ = writeln!(s, "  enabled:        {enabled}");
            let advertised = match &settings.advertised {
                Ok(Some(origin)) => origin.clone(),
                Ok(None) => "(not set)".to_string(),
                Err(reason) => format!("invalid: {reason}"),
            };
            let _ = writeln!(s, "  advertised url: {advertised}");
            let gateway = match status.gateway {
                Some(GatewayHealth::Running) => "running",
                Some(GatewayHealth::NotRunning) => "not running",
                Some(GatewayHealth::Unexpected) => "port in use by something else",
                None if !settings.enabled => "not checked (remote access is off)",
                None => "unknown",
            };
            let _ = writeln!(
                s,
                "  gateway:        {gateway} (127.0.0.1:{})",
                settings.port
            );
        }
        Err(reason) => {
            let _ = writeln!(s, "  settings:       cannot be read: {reason}");
            let _ = writeln!(s, "  gateway:        unknown (no port without settings)");
        }
    }
    if status.workspaces.is_empty() {
        let _ = writeln!(s, "  workspaces:     none served with remote access on");
    } else {
        let _ = writeln!(s, "  workspaces:");
        for (label, id) in &status.workspaces {
            let _ = writeln!(s, "    {label} ({id})");
        }
    }
    match &status.devices {
        Ok((active, revoked)) => {
            let _ = writeln!(s, "  devices:        {active} paired, {revoked} revoked");
        }
        Err(reason) => {
            let _ = writeln!(s, "  devices:        cannot be read: {reason}");
        }
    }
    s
}
