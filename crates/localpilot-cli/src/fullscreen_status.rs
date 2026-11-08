//! The `/remote` status canvas in the full-screen terminal.
//!
//! The probed rows (config, gateway, phone address, device counts) come from
//! one bounded check on the devices worker, merged axis by axis into a
//! `StatusModel` (see `remote_status_model`); the live rows (relay, this
//! session, its work, who is attached here) come from the host's own state at
//! every tick. Each opening of the canvas is a generation: closing,
//! replacing it or a session change retires it (its check's network part is
//! cancelled; the worker keeps owning the check until it answers), and an
//! answer for a retired generation never reaches a newer canvas.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent};
use localpilot_core::SessionId;
use localpilot_terminal_ui::{AppModel, StatusRow, StatusView};

use crate::remote_devices::{Devices, StatusAsked};
use crate::remote_share::ShareState;
use crate::remote_status_model::{AddressView, ConfigView, StatusModel};

/// An action the canvas offers, by what it does (its label is drawn from
/// this, and Enter acts on the one drawn).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StatusAction {
    Pair,
    Devices,
    Share,
    Stop,
    Refresh,
    Close,
    /// One immediate reconnect attempt, for an interrupted share.
    Reconnect,
    /// No server ran when sharing was asked for: start it, then share.
    StartAndShare,
    /// The server stopped under an interrupted share: start it, then
    /// reconnect.
    StartAndReconnect,
}

impl StatusAction {
    fn label(self) -> &'static str {
        match self {
            Self::Pair => "Pair device",
            Self::Devices => "Devices",
            Self::Share => "Share session",
            Self::Stop => "Stop sharing",
            Self::Refresh => "Refresh status",
            Self::Close => "Close",
            Self::Reconnect => "Reconnect now",
            Self::StartAndShare => "Start server and share",
            Self::StartAndReconnect => "Start server and reconnect",
        }
    }
}

/// What this session is doing, from the host's authoritative state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Work {
    Idle,
    Working,
    WaitingForPermission,
    WaitingForAnswer,
}

/// What the canvas reads from the host at each tick.
pub(super) struct StatusInputs<'a> {
    pub(super) session: Option<SessionId>,
    pub(super) share: ShareState,
    pub(super) retry_at: Option<Instant>,
    /// This session's confirmed attendance (only while shared).
    pub(super) attended: Option<&'a [localpilot_rpc::AttendedDevice]>,
    pub(super) work: Work,
    /// The current sharing intent found no background server.
    pub(super) server_needed: bool,
}

/// The host's status-canvas state.
pub(super) struct StatusHost {
    cwd: PathBuf,
    model: RefCell<StatusModel>,
    /// The canvas shown (`0` when none), counted per opening.
    generation: Cell<u64>,
    opened: Cell<u64>,
    session: Cell<Option<SessionId>>,
    /// The registry change count the model last saw.
    seen_epoch: Cell<u64>,
    /// The check outstanding was already marked as taking too long.
    late_marked: Cell<bool>,
    /// The actions as last drawn, in order (Enter acts on these).
    drawn: RefCell<Vec<StatusAction>>,
    /// The canvas has not been drawn since it opened: only then is the
    /// state's own first action selected. A state change never moves the
    /// selection (it stays on the same action by label).
    fresh: Cell<bool>,
}

/// Which state's own first action the canvas offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Primary {
    None,
    Reconnect,
    StartAndShare,
    StartAndReconnect,
}

impl StatusHost {
    pub(super) fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            model: RefCell::new(StatusModel::default()),
            generation: Cell::new(0),
            opened: Cell::new(0),
            session: Cell::new(None),
            seen_epoch: Cell::new(0),
            late_marked: Cell::new(false),
            drawn: RefCell::new(Vec::new()),
            fresh: Cell::new(false),
        }
    }

    /// `/remote`: open the canvas and check status.
    pub(super) fn open(
        &self,
        app: &mut AppModel,
        devices: &mut Devices,
        session: Option<SessionId>,
    ) {
        self.retire(devices);
        let generation = self.opened.get() + 1;
        self.opened.set(generation);
        self.generation.set(generation);
        self.session.set(session);
        self.late_marked.set(false);
        let asked = devices.check_status(generation, &self.cwd);
        self.not_started(asked);
        self.fresh.set(true);
        app.open_status_view(StatusView::new("Remote".to_string()));
    }

    /// A check that could not even be started leaves nothing checking: the
    /// probed rows say so (keeping their last known values).
    fn not_started(&self, asked: StatusAsked) {
        if asked == StatusAsked::Unavailable {
            self.model.borrow_mut().check_lost(
                "the check could not start (the background worker is unavailable); try Refresh",
            );
        }
    }

    /// The canvas went (closed, replaced or the session changed).
    fn retire(&self, devices: &mut Devices) {
        let generation = self.generation.replace(0);
        if generation != 0 {
            devices.retire_status(generation);
        }
    }

    /// Route a key to the canvas. Returns the action chosen, if any; the
    /// caller performs Pair, Devices, Share and Stop.
    pub(super) fn key(
        &self,
        app: &mut AppModel,
        devices: &mut Devices,
        key: KeyEvent,
    ) -> Option<StatusAction> {
        let view = app.status_view_mut()?;
        if !view.controls_visible() && key.code != KeyCode::Esc {
            return None;
        }
        match key.code {
            KeyCode::Esc => {
                app.close_status_view();
                self.retire(devices);
                None
            }
            KeyCode::Tab | KeyCode::Down => {
                view.move_action(true);
                None
            }
            KeyCode::BackTab | KeyCode::Up => {
                view.move_action(false);
                None
            }
            KeyCode::PageDown => {
                view.page_down();
                None
            }
            KeyCode::PageUp => {
                view.page_up();
                None
            }
            KeyCode::Enter => {
                // Exactly the action drawn at that place.
                let action = self.drawn.borrow().get(view.action).copied()?;
                match action {
                    StatusAction::Refresh => {
                        let generation = self.generation.get();
                        let asked = devices.check_status(generation, &self.cwd);
                        if asked == StatusAsked::Sent {
                            self.late_marked.set(false);
                        }
                        self.not_started(asked);
                    }
                    StatusAction::Close => {
                        app.close_status_view();
                        self.retire(devices);
                    }
                    StatusAction::Pair | StatusAction::Devices => {
                        app.close_status_view();
                        self.retire(devices);
                    }
                    StatusAction::Share
                    | StatusAction::Stop
                    | StatusAction::Reconnect
                    | StatusAction::StartAndShare
                    | StatusAction::StartAndReconnect => {}
                }
                Some(action)
            }
            _ => None,
        }
    }

    /// Every tick: retire a replaced canvas or one from another session,
    /// take this canvas's check, and keep it current.
    pub(super) fn tick(
        &self,
        app: &mut AppModel,
        devices: &mut Devices,
        inputs: &StatusInputs<'_>,
    ) {
        // The registry changed (a revoke accepted, a device paired): counts
        // read before are not current. Once per change, never per frame.
        let epoch = devices.epoch();
        if epoch != self.seen_epoch.replace(epoch) {
            self.model.borrow_mut().registry_changed();
        }
        if app.status_view().is_none() {
            self.retire(devices);
            return;
        }
        if self.session.get() != inputs.session || app.pairing_replaced() {
            app.close_status_view();
            self.retire(devices);
            return;
        }
        let generation = self.generation.get();
        match devices.take_status(generation) {
            Some(Some(probe)) => {
                self.model.borrow_mut().merge(probe, epoch);
                self.late_marked.set(false);
            }
            Some(None) => self
                .model
                .borrow_mut()
                .check_lost("the check did not finish"),
            None => {
                if devices.status_late(Instant::now()) && !self.late_marked.replace(true) {
                    self.model
                        .borrow_mut()
                        .check_lost("the check is taking too long");
                }
            }
        }
        let checking = devices.checking_status(generation);
        self.render(app, inputs, checking);
    }

    fn render(&self, app: &mut AppModel, inputs: &StatusInputs<'_>, checking: bool) {
        let model = self.model.borrow();
        let mut rows = Vec::new();
        let row = |label: &str, value: String| StatusRow {
            label: label.to_string(),
            value,
        };
        if checking {
            rows.push(row("Check", "Checking remote access...".to_string()));
        }
        rows.push(row("Relay", relay_text(&inputs.share, inputs.retry_at)));
        rows.push(row("Config", config_text(&model)));
        rows.push(row("Gateway", model.gateway_line()));
        rows.push(row("Phone address", address_text(&model)));
        let attached_here = inputs.attended.is_some_and(|devices| !devices.is_empty());
        rows.push(row(
            "Pairing",
            if attached_here {
                format!("{}; a device is connected here", model.reachability())
            } else {
                model.reachability().to_string()
            },
        ));
        rows.push(row(
            "This session",
            format!("{}; running in this terminal", session_text(&inputs.share)),
        ));
        rows.push(row("Work", work_text(inputs.work).to_string()));
        match inputs.share {
            ShareState::Interrupted { .. } => rows.push(row(
                "Phone actions",
                "Unavailable until sharing reconnects. Local work continues in this terminal."
                    .to_string(),
            )),
            ShareState::ServerStopped => rows.push(row(
                "Phone actions",
                "Unavailable: the background server is not running. Local work continues in \
                 this terminal."
                    .to_string(),
            )),
            _ if inputs.server_needed => rows.push(row(
                "Background server",
                "Not running for this workspace; sharing needs it. Starting it does not start \
                 the gateway."
                    .to_string(),
            )),
            _ => {}
        }
        match inputs.attended {
            None => rows.push(row(
                "Attached here",
                "Unknown - connection status unavailable".to_string(),
            )),
            Some([]) => rows.push(row(
                "Attached here",
                "No devices attached to this session.".to_string(),
            )),
            Some(devices) => {
                let width = short_width(devices);
                for (index, device) in devices.iter().enumerate() {
                    let name = device.name.as_deref().map_or_else(
                        || "(name unknown)".to_string(),
                        localpilot_terminal_ui::sanitize_text,
                    );
                    let short: String = device.id.chars().take(width).collect();
                    let scope = match device.scope {
                        Some(localpilot_rpc::Scope::Control) => "control",
                        Some(localpilot_rpc::Scope::Observe) => "observe",
                        None => "scope unknown",
                    };
                    let label = if index == 0 { "Attached here" } else { "" };
                    let standing = match device.standing {
                        localpilot_rpc::DeviceStanding::Active => String::new(),
                        localpilot_rpc::DeviceStanding::Unknown => {
                            ", access unconfirmed (the device registry cannot be read)".to_string()
                        }
                        localpilot_rpc::DeviceStanding::NotGranted => {
                            ", no longer allowed in this workspace, being disconnected".to_string()
                        }
                        localpilot_rpc::DeviceStanding::Revoked => {
                            ", revoked, being disconnected".to_string()
                        }
                        localpilot_rpc::DeviceStanding::Removed => {
                            ", no longer paired, being disconnected".to_string()
                        }
                    };
                    rows.push(row(label, format!("{name} [{short}], {scope}{standing}")));
                }
            }
        }
        rows.push(row("Devices", model.counts_line()));
        if !matches!(inputs.share, ShareState::Off) {
            rows.push(row(
                "Note",
                "Closing this terminal ends phone access.".to_string(),
            ));
        }
        drop(model);
        let (primary, actions) = actions_for(&inputs.share, inputs.server_needed);
        let Some(view) = app.status_view_mut() else {
            return;
        };
        view.rows = rows;
        view.set_actions(
            actions
                .iter()
                .map(|action| action.label().to_string())
                .collect(),
        );
        // Only a fresh canvas selects its state's own first action; after
        // that the user's selection (kept by label) stays whatever the state
        // does.
        if self.fresh.replace(false) && primary != Primary::None {
            view.action = 0;
        }
        *self.drawn.borrow_mut() = actions;
    }
}

/// The actions for a sharing state, its own first.
fn actions_for(share: &ShareState, server_needed: bool) -> (Primary, Vec<StatusAction>) {
    use StatusAction::{
        Close, Devices, Pair, Reconnect, Refresh, Share, StartAndReconnect, StartAndShare, Stop,
    };
    match share {
        ShareState::Interrupted { .. } => (
            Primary::Reconnect,
            vec![Reconnect, Stop, Pair, Devices, Refresh, Close],
        ),
        ShareState::ServerStopped => (
            Primary::StartAndReconnect,
            vec![StartAndReconnect, Stop, Pair, Devices, Refresh, Close],
        ),
        ShareState::Off if server_needed => (
            Primary::StartAndShare,
            vec![StartAndShare, Pair, Devices, Refresh, Close],
        ),
        ShareState::Off => (Primary::None, vec![Pair, Devices, Share, Refresh, Close]),
        ShareState::Connecting | ShareState::Shared => {
            (Primary::None, vec![Pair, Devices, Stop, Refresh, Close])
        }
    }
}

/// The shortest id prefix (at least 4) that tells the attached devices apart.
fn short_width(devices: &[localpilot_rpc::AttendedDevice]) -> usize {
    let longest = devices
        .iter()
        .map(|device| device.id.chars().count())
        .max()
        .unwrap_or(0);
    (4..=longest.max(4))
        .find(|width| {
            let mut seen = std::collections::HashSet::new();
            devices
                .iter()
                .all(|device| seen.insert(device.id.chars().take(*width).collect::<String>()))
        })
        .unwrap_or(longest)
}

fn relay_text(share: &ShareState, retry_at: Option<Instant>) -> String {
    match share {
        ShareState::Off => "Off".to_string(),
        ShareState::Connecting => "Connecting to the workspace server...".to_string(),
        ShareState::Shared => "Connected to workspace server".to_string(),
        ShareState::Interrupted { .. } => match retry_at {
            Some(at) => format!(
                "Interrupted; reconnecting in {}s",
                at.saturating_duration_since(Instant::now())
                    .as_secs()
                    .max(1)
            ),
            None => "Interrupted; reconnecting".to_string(),
        },
        ShareState::ServerStopped => {
            "Interrupted; the background server is not running".to_string()
        }
    }
}

fn session_text(share: &ShareState) -> &'static str {
    match share {
        ShareState::Off => "Not shared",
        ShareState::Connecting => "Sharing: connecting",
        ShareState::Shared => "Shared",
        ShareState::Interrupted { .. } | ShareState::ServerStopped => "Sharing interrupted",
    }
}

fn work_text(work: Work) -> &'static str {
    match work {
        Work::Idle => "Idle",
        Work::Working => "Working",
        Work::WaitingForPermission => "Waiting for permission",
        Work::WaitingForAnswer => "Waiting for your answer",
    }
}

fn config_text(model: &StatusModel) -> String {
    let checked = model.config.last.as_ref();
    let text = |config: &ConfigView| {
        if config.enabled {
            format!("Remote access enabled (port {})", config.port)
        } else {
            "Remote access disabled in config".to_string()
        }
    };
    match (checked, model.config.current, &model.config.error) {
        (Some(checked), true, _) => text(&checked.value),
        (Some(checked), false, error) => format!(
            "Unknown - {} (last known: {}, checked {})",
            error.as_deref().unwrap_or("not current"),
            text(&checked.value),
            crate::remote_status_model::utc(checked.at)
        ),
        (None, _, Some(error)) => format!("Unknown - {error}"),
        (None, _, None) => "Checking...".to_string(),
    }
}

fn address_text(model: &StatusModel) -> String {
    let text = |address: &AddressView| match address {
        AddressView::Origin(origin) => origin.clone(),
        AddressView::NotConfigured => "Not configured".to_string(),
        AddressView::Invalid(why) => format!("Invalid HTTPS address: {why}"),
    };
    match (
        model.address.last.as_ref(),
        model.address.current,
        &model.address.error,
    ) {
        (Some(checked), true, _) => text(&checked.value),
        (Some(checked), false, error) => format!(
            "Unknown - {} (last known: {}, checked {})",
            error.as_deref().unwrap_or("not current"),
            text(&checked.value),
            crate::remote_status_model::utc(checked.at)
        ),
        (None, _, Some(error)) => format!("Unknown - {error}"),
        (None, _, None) => "Checking...".to_string(),
    }
}
