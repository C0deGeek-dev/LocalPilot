//! What the terminal's `/remote` status canvas knows, axis by axis.
//!
//! Each probed axis (config, gateway, phone address, device counts) keeps its
//! own last good value, where it was checked against (the port, origin and
//! whether remote access was on), when, and whether it is current. A probe
//! merges axis by axis: a good axis replaces its value; a failed one keeps the
//! old value as last known, beside the current error, so one failure never
//! blanks the other rows. Device counts made before a known change to the
//! registry (a revoke that may have committed, a device paired) are never
//! current again.

#![cfg_attr(not(feature = "tui"), allow(dead_code))]

use std::time::SystemTime;

use crate::remote_status::GatewayHealth;

/// What a value was checked against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Provenance {
    pub(crate) enabled: bool,
    pub(crate) port: u16,
    pub(crate) origin: Option<String>,
}

/// A value, where it was checked, and when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Checked<T> {
    pub(crate) value: T,
    pub(crate) provenance: Option<Provenance>,
    pub(crate) at: SystemTime,
}

/// One axis: the last good value, whether it is current, and the current
/// error if the latest check failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Axis<T> {
    pub(crate) last: Option<Checked<T>>,
    pub(crate) current: bool,
    pub(crate) error: Option<String>,
}

impl<T> Default for Axis<T> {
    fn default() -> Self {
        Self {
            last: None,
            current: false,
            error: None,
        }
    }
}

impl<T> Axis<T> {
    fn good(&mut self, value: T, provenance: Option<Provenance>, at: SystemTime) {
        self.last = Some(Checked {
            value,
            provenance,
            at,
        });
        self.current = true;
        self.error = None;
    }

    /// The check failed: the old value stays, as last known.
    fn failed(&mut self, why: String) {
        self.current = false;
        self.error = Some(bounded(&why));
    }

    /// The current value, if there is one.
    pub(crate) fn now(&self) -> Option<&T> {
        self.last
            .as_ref()
            .filter(|_| self.current)
            .map(|checked| &checked.value)
    }
}

/// The config file's remote settings (never a claim about what a running
/// gateway or server uses).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigView {
    pub(crate) enabled: bool,
    pub(crate) port: u16,
}

/// The gateway, as checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GatewayView {
    Health(GatewayHealth),
    /// Remote access is off in the config, so nothing was checked (a
    /// gateway may still be running).
    NotChecked,
}

/// The phone address, as configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AddressView {
    Origin(String),
    NotConfigured,
    Invalid(String),
}

/// One probe's result, with what it was asked for.
#[derive(Debug, Clone)]
pub(crate) struct Probe {
    /// The status, each part stamped with when it was observed.
    pub(crate) observed: crate::remote_status::ObservedStatus,
    /// The registry change count when the probe was asked for.
    pub(crate) epoch: u64,
}

/// What the status canvas knows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StatusModel {
    pub(crate) config: Axis<ConfigView>,
    pub(crate) gateway: Axis<GatewayView>,
    pub(crate) address: Axis<AddressView>,
    /// Active and revoked registry records.
    pub(crate) counts: Axis<(usize, usize)>,
}

/// Text from outside (errors, addresses) for display: one line, bounded.
fn bounded(text: &str) -> String {
    let clean: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut end = clean.len().min(240);
    while !clean.is_char_boundary(end) {
        end -= 1;
    }
    if end < clean.len() {
        format!("{}…", &clean[..end])
    } else {
        clean
    }
}

impl StatusModel {
    /// Merge a probe made at registry change `probe.epoch`, the registry
    /// having changed `epoch` times by now.
    pub(crate) fn merge(&mut self, probe: Probe, epoch: u64) {
        let observed = probe.observed;
        let settings_at = observed.settings_at;
        let gateway_at = observed.gateway_at;
        let devices_at = observed.devices_at;
        let status = observed.status;
        match &status.settings {
            Ok(settings) => {
                let provenance = Provenance {
                    enabled: settings.enabled,
                    port: settings.port,
                    origin: settings
                        .advertised
                        .clone()
                        .ok()
                        .flatten()
                        .map(|o| bounded(&o)),
                };
                self.config.good(
                    ConfigView {
                        enabled: settings.enabled,
                        port: settings.port,
                    },
                    Some(provenance.clone()),
                    settings_at,
                );
                let address = match &settings.advertised {
                    Ok(Some(origin)) => AddressView::Origin(bounded(origin)),
                    Ok(None) => AddressView::NotConfigured,
                    Err(why) => AddressView::Invalid(bounded(why)),
                };
                self.address
                    .good(address, Some(provenance.clone()), settings_at);
                match &status.gateway {
                    None if !settings.enabled => {
                        self.gateway
                            .good(GatewayView::NotChecked, Some(provenance), settings_at);
                    }
                    Some(
                        health @ (GatewayHealth::Running
                        | GatewayHealth::NotRunning
                        | GatewayHealth::Unexpected),
                    ) => {
                        self.gateway.good(
                            GatewayView::Health(health.clone()),
                            Some(provenance),
                            gateway_at,
                        );
                    }
                    Some(GatewayHealth::TimedOut) => {
                        self.gateway.failed("no answer in time".to_string());
                    }
                    Some(GatewayHealth::CheckFailed(why)) => self.gateway.failed(why.clone()),
                    None => self.gateway.failed("not checked".to_string()),
                }
            }
            Err(why) => {
                let why = format!("settings could not be read: {why}");
                self.config.failed(why.clone());
                self.address.failed(why.clone());
                self.gateway.failed(why);
            }
        }
        match &status.devices {
            Ok(counts) if probe.epoch == epoch => self.counts.good(*counts, None, devices_at),
            Ok(counts) => {
                // Read before a change that may have committed: kept as last
                // known, never current.
                self.counts.good(*counts, None, devices_at);
                self.counts.current = false;
                self.counts.error = Some("changed since this check".to_string());
            }
            Err(why) => self.counts.failed(why.clone()),
        }
    }

    /// A whole check did not finish (it timed out, or the worker went): every
    /// probed axis is uncertain, keeping its last known value.
    pub(crate) fn check_lost(&mut self, why: &str) {
        self.config.failed(why.to_string());
        self.gateway.failed(why.to_string());
        self.address.failed(why.to_string());
        self.counts.failed(why.to_string());
    }

    /// The registry changed (or may have): the counts are no longer current.
    pub(crate) fn registry_changed(&mut self) {
        if self.counts.current {
            self.counts.current = false;
            self.counts.error = Some("changed since this check".to_string());
        }
    }

    /// Whether a phone could reach the gateway, as far as this terminal
    /// knows (it never checks the phone side).
    pub(crate) fn reachability(&self) -> &'static str {
        match (self.config.now(), self.gateway.now(), self.address.now()) {
            (Some(ConfigView { enabled: false, .. }), _, _) => {
                "Unavailable - remote access is disabled in config"
            }
            (_, _, Some(AddressView::NotConfigured)) => "Needs a trusted HTTPS address",
            (_, _, Some(AddressView::Invalid(_))) => "Needs a valid HTTPS address",
            (_, Some(GatewayView::Health(GatewayHealth::NotRunning)), _) => {
                "Unavailable - gateway is off"
            }
            (_, Some(GatewayView::Health(GatewayHealth::Unexpected)), _) => {
                "Unavailable - another service uses the gateway port"
            }
            (
                Some(_),
                Some(GatewayView::Health(GatewayHealth::Running)),
                Some(AddressView::Origin(_)),
            ) => "Address set; phone connection not checked",
            _ => "Unknown - gateway or address not current",
        }
    }

    /// The gateway row: the current value, or the last known one labelled
    /// with the port it was checked on, and the current error.
    pub(crate) fn gateway_line(&self) -> String {
        let shown = self.gateway.last.as_ref().map(|checked| {
            let port = checked.provenance.as_ref().map_or(0, |p| p.port);
            match &checked.value {
                GatewayView::NotChecked => {
                    "Not checked - remote access is disabled in config".to_string()
                }
                GatewayView::Health(GatewayHealth::Running) => {
                    format!("Running at 127.0.0.1:{port}")
                }
                GatewayView::Health(GatewayHealth::NotRunning) => {
                    format!("Off at 127.0.0.1:{port}")
                }
                GatewayView::Health(GatewayHealth::Unexpected) => {
                    format!("Unavailable - another service uses 127.0.0.1:{port}")
                }
                GatewayView::Health(GatewayHealth::TimedOut) => {
                    "Unknown - no answer in time".to_string()
                }
                GatewayView::Health(GatewayHealth::CheckFailed(why)) => format!("Unknown - {why}"),
            }
        });
        match (shown, self.gateway.current, &self.gateway.error) {
            (Some(shown), true, _) => shown,
            (Some(shown), false, Some(error)) => {
                format!(
                    "Unknown - {error} (last known: {shown}, checked {})",
                    self.gateway_checked()
                )
            }
            (Some(shown), false, None) => format!("Last known: {shown}"),
            (None, _, Some(error)) => format!("Unknown - {error}"),
            (None, _, None) => "Checking...".to_string(),
        }
    }

    fn gateway_checked(&self) -> String {
        self.gateway
            .last
            .as_ref()
            .map_or_else(String::new, |checked| utc(checked.at))
    }

    /// The devices row.
    pub(crate) fn counts_line(&self) -> String {
        match (&self.counts.last, self.counts.current, &self.counts.error) {
            (Some(checked), true, _) => {
                format!("{} paired, {} revoked", checked.value.0, checked.value.1)
            }
            (Some(checked), false, error) => format!(
                "Last known: {} paired, {} revoked (checked {}{})",
                checked.value.0,
                checked.value.1,
                utc(checked.at),
                error
                    .as_ref()
                    .map_or_else(String::new, |e| format!("; {e}"))
            ),
            (None, _, Some(error)) => {
                format!("Unavailable - device list could not be read: {error}")
            }
            (None, _, None) => "Checking...".to_string(),
        }
    }
}

/// A time as `YYYY-MM-DD HH:MM UTC`.
pub(crate) fn utc(at: SystemTime) -> String {
    let unix = at
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let days = i64::try_from(unix / 86_400).unwrap_or(i64::MAX);
    let seconds = unix % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        seconds / 3_600,
        (seconds % 3_600) / 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_status::{RemoteSettings, RemoteStatus};
    use std::time::Duration;

    fn status(
        settings: Result<RemoteSettings, String>,
        gateway: Option<GatewayHealth>,
        devices: Result<(usize, usize), String>,
    ) -> RemoteStatus {
        RemoteStatus {
            settings,
            gateway,
            workspaces: Vec::new(),
            devices,
        }
    }

    fn settings(enabled: bool, port: u16, origin: Option<&str>) -> Result<RemoteSettings, String> {
        Ok(RemoteSettings {
            enabled,
            advertised: Ok(origin.map(str::to_string)),
            port,
        })
    }

    fn probe(status: RemoteStatus, epoch: u64, minutes: u64) -> Probe {
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(minutes * 60);
        Probe {
            observed: crate::remote_status::ObservedStatus {
                status,
                settings_at: at,
                gateway_at: at,
                devices_at: at,
            },
            epoch,
        }
    }

    #[test]
    fn each_axis_keeps_the_time_its_own_source_was_observed() {
        let mut model = StatusModel::default();
        let at = |seconds| SystemTime::UNIX_EPOCH + Duration::from_secs(seconds);
        model.merge(
            Probe {
                observed: crate::remote_status::ObservedStatus {
                    status: status(
                        settings(true, 9417, Some("https://a.example")),
                        Some(GatewayHealth::Running),
                        Ok((1, 0)),
                    ),
                    settings_at: at(60),
                    gateway_at: at(120),
                    devices_at: at(180),
                },
                epoch: 0,
            },
            0,
        );
        assert_eq!(model.config.last.as_ref().unwrap().at, at(60));
        assert_eq!(model.address.last.as_ref().unwrap().at, at(60));
        assert_eq!(model.gateway.last.as_ref().unwrap().at, at(120));
        assert_eq!(model.counts.last.as_ref().unwrap().at, at(180));
        // A later failed check keeps those times on the retained values.
        model.merge(
            Probe {
                observed: crate::remote_status::ObservedStatus {
                    status: status(Err("bad".into()), None, Err("locked".into())),
                    settings_at: at(600),
                    gateway_at: at(600),
                    devices_at: at(600),
                },
                epoch: 0,
            },
            0,
        );
        assert_eq!(model.gateway.last.as_ref().unwrap().at, at(120));
        assert_eq!(model.counts.last.as_ref().unwrap().at, at(180));
        assert!(
            model
                .gateway_line()
                .contains("checked 1970-01-01 00:02 UTC"),
            "{}",
            model.gateway_line()
        );
        // A whole check lost: everything uncertain, nothing dropped.
        let mut lost = StatusModel::default();
        lost.merge(
            probe(
                status(
                    settings(true, 1, Some("https://a.example")),
                    Some(GatewayHealth::Running),
                    Ok((2, 0)),
                ),
                0,
                1,
            ),
            0,
        );
        lost.check_lost("the check did not finish");
        assert!(lost.config.now().is_none() && lost.gateway.now().is_none());
        assert!(lost.counts_line().starts_with("Last known: 2 paired"));
        assert_eq!(
            lost.reachability(),
            "Unknown - gateway or address not current"
        );
    }

    #[test]
    fn a_failed_axis_keeps_its_last_value_and_the_others_stay_current() {
        let mut model = StatusModel::default();
        model.merge(
            probe(
                status(
                    settings(true, 9417, Some("https://pc.example")),
                    Some(GatewayHealth::Running),
                    Ok((3, 1)),
                ),
                0,
                1,
            ),
            0,
        );
        assert_eq!(model.gateway_line(), "Running at 127.0.0.1:9417");
        assert_eq!(
            model.reachability(),
            "Address set; phone connection not checked"
        );
        model.merge(
            probe(
                status(
                    settings(true, 9417, Some("https://pc.example")),
                    Some(GatewayHealth::TimedOut),
                    Ok((4, 1)),
                ),
                0,
                2,
            ),
            0,
        );
        let line = model.gateway_line();
        assert!(line.starts_with("Unknown - no answer in time"), "{line}");
        assert!(
            line.contains("last known: Running at 127.0.0.1:9417, checked 1970-01-01 00:01 UTC"),
            "{line}"
        );
        assert_eq!(
            model.counts_line(),
            "4 paired, 1 revoked",
            "a good count stays current"
        );
        assert_eq!(
            model.reachability(),
            "Unknown - gateway or address not current"
        );
    }

    #[test]
    fn a_last_known_gateway_names_the_port_it_was_checked_on() {
        let mut model = StatusModel::default();
        model.merge(
            probe(
                status(
                    settings(true, 9417, Some("https://a.example")),
                    Some(GatewayHealth::Running),
                    Ok((1, 0)),
                ),
                0,
                1,
            ),
            0,
        );
        // The config moves to another port, which does not answer.
        model.merge(
            probe(
                status(
                    settings(true, 9500, Some("https://a.example")),
                    Some(GatewayHealth::TimedOut),
                    Ok((1, 0)),
                ),
                0,
                2,
            ),
            0,
        );
        let line = model.gateway_line();
        assert!(line.contains("9417"), "the port it was checked on: {line}");
        assert!(!line.contains("Running at 127.0.0.1:9500"), "{line}");
        assert_eq!(
            model.config.now(),
            Some(&ConfigView {
                enabled: true,
                port: 9500
            })
        );
    }

    #[test]
    fn disabled_config_is_not_checked_rather_than_off() {
        let mut model = StatusModel::default();
        model.merge(
            probe(
                status(
                    settings(true, 9417, Some("https://a.example")),
                    Some(GatewayHealth::Running),
                    Ok((1, 0)),
                ),
                0,
                1,
            ),
            0,
        );
        model.merge(
            probe(
                status(
                    settings(false, 9417, Some("https://a.example")),
                    None,
                    Ok((1, 0)),
                ),
                0,
                2,
            ),
            0,
        );
        assert_eq!(
            model.gateway_line(),
            "Not checked - remote access is disabled in config"
        );
        assert!(!model.gateway_line().contains("Off"));
        assert_eq!(
            model.reachability(),
            "Unavailable - remote access is disabled in config"
        );
        // Unreadable settings keep everything as last known.
        model.merge(
            probe(status(Err("bad toml".into()), None, Ok((1, 0))), 0, 3),
            0,
        );
        assert!(model
            .gateway_line()
            .starts_with("Unknown - settings could not be read: bad toml"));
        assert_eq!(
            model.counts_line(),
            "1 paired, 0 revoked",
            "the registry was read fine"
        );
    }

    #[test]
    fn counts_from_before_a_registry_change_are_never_current() {
        let mut model = StatusModel::default();
        model.merge(
            probe(
                status(
                    settings(true, 1, None),
                    Some(GatewayHealth::Running),
                    Ok((300, 12)),
                ),
                0,
                1,
            ),
            0,
        );
        assert_eq!(model.counts_line(), "300 paired, 12 revoked");
        // A revoke was accepted (it may commit): the counts go stale.
        model.registry_changed();
        assert!(model
            .counts_line()
            .starts_with("Last known: 300 paired, 12 revoked"));
        // A probe asked before that change returns late: still not current.
        model.merge(
            probe(
                status(
                    settings(true, 1, None),
                    Some(GatewayHealth::Running),
                    Ok((300, 12)),
                ),
                0,
                2,
            ),
            1,
        );
        assert!(
            model.counts_line().starts_with("Last known"),
            "{}",
            model.counts_line()
        );
        // One asked after it is.
        model.merge(
            probe(
                status(
                    settings(true, 1, None),
                    Some(GatewayHealth::Running),
                    Ok((299, 13)),
                ),
                1,
                3,
            ),
            1,
        );
        assert_eq!(model.counts_line(), "299 paired, 13 revoked");
        // A failed read keeps the last value, labelled.
        model.merge(
            probe(
                status(
                    settings(true, 1, None),
                    Some(GatewayHealth::Running),
                    Err("locked".into()),
                ),
                1,
                4,
            ),
            1,
        );
        assert!(model
            .counts_line()
            .contains("Last known: 299 paired, 13 revoked"));
        assert!(model.counts_line().contains("locked"));
    }

    #[test]
    fn error_text_is_one_bounded_line() {
        let mut model = StatusModel::default();
        model.merge(
            probe(
                status(
                    settings(true, 1, None),
                    Some(GatewayHealth::CheckFailed(format!(
                        "x\u{1b}[2J{}",
                        "y".repeat(500)
                    ))),
                    Ok((0, 0)),
                ),
                0,
                1,
            ),
            0,
        );
        let line = model.gateway_line();
        assert!(!line.contains('\u{1b}'));
        assert!(line.len() < 300, "{}", line.len());
    }
}
