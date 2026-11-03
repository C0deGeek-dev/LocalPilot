//! The paired devices in the full-screen terminal: `/remote devices`.
//!
//! The list and every revoke go through one controller (see
//! `remote_devices`), kept by the host for the whole terminal: closing or
//! replacing the view never cancels a revoke, and its answer is told once,
//! in the view when it is on screen or as a notice otherwise. A confirmation
//! names the device by the full id it was opened for, starts on Cancel, and
//! never changes under live updates.

use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent};
use localpilot_core::SessionId;
use localpilot_terminal_ui::{
    AppModel, DevicesConfirm, DevicesFocus, DevicesRow, DevicesView, RuntimeUpdate,
};

use crate::remote_devices::{Devices, Listing, Refused, RevokeState};
use crate::server_devices::{DeviceGrant, DeviceReach, DeviceRegistry, DeviceRow, RevokeOutcome};

const REVOKE: &str = "Revoke device...";
const REVOKE_UNAVAILABLE: &str = "Revoke device (unavailable)";
const PAIR: &str = "Pair device";
const REFRESH: &str = "Refresh";
const CLOSE: &str = "Close";
const CANCEL: &str = "Cancel";
const CONFIRM: &str = "Revoke device";
const BACK: &str = "Back";

/// How long a revoked device may still show attached here before the view
/// says its disconnect is not confirmed (what the terminal can see, not a
/// promise of the server).
const DISCONNECT_SEEN: Duration = Duration::from_secs(10);

/// What a key in the devices view asks the terminal to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DevicesKey {
    Handled,
    /// Open pairing (the view is already closed).
    Pair,
}

/// A revoke's answer, as the view keeps saying it.
#[derive(PartialEq, Eq)]
struct Told {
    op: u64,
    state: RevokeState,
    /// Whether an answer could still come (unknown and waiting, versus
    /// unknown with the worker gone).
    in_flight: bool,
}

/// The host's devices state.
pub(super) struct DevicesHost {
    devices: RefCell<Devices>,
    /// The device selected, by full id.
    selected: RefCell<Option<String>>,
    /// The device a confirmation was opened for (frozen).
    confirm: RefCell<Option<DeviceRow>>,
    /// The session the view was opened for.
    session: Cell<Option<SessionId>>,
    /// The last revoke state the user was actually told.
    told: RefCell<Option<Told>>,
    /// What the last revoke came to, kept apart from the list.
    result: RefCell<Option<String>>,
    /// A revoked device to follow here: id, name, when revoked.
    watch: RefCell<Option<(String, String, Instant)>>,
    /// A short note in the view.
    note: RefCell<Option<String>>,
    /// This session's confirmed attendance at the last tick.
    attached: RefCell<Option<Vec<String>>>,
    /// The selected device left the list: nothing is selected until the
    /// user picks a device again, and the view keeps saying why.
    lost: Cell<bool>,
}

impl DevicesHost {
    pub(super) fn new(registry: Arc<DeviceRegistry>) -> Self {
        Self {
            devices: RefCell::new(Devices::new(registry)),
            selected: RefCell::new(None),
            confirm: RefCell::new(None),
            session: Cell::new(None),
            told: RefCell::new(None),
            result: RefCell::new(None),
            watch: RefCell::new(None),
            note: RefCell::new(None),
            attached: RefCell::new(None),
            lost: Cell::new(false),
        }
    }

    /// The same host over `devices` (tests).
    #[cfg(test)]
    pub(super) fn with_devices(devices: Devices) -> Self {
        let host = Self::new(Arc::new(DeviceRegistry::default()));
        *host.devices.borrow_mut() = devices;
        host
    }

    /// The controller (the one worker, its reads, revokes and status checks).
    pub(super) fn controller(&self) -> std::cell::RefMut<'_, Devices> {
        self.devices.borrow_mut()
    }

    /// `/remote devices`: open the view and read the list.
    pub(super) fn open(&self, app: &mut AppModel, session: Option<SessionId>) {
        self.session.set(session);
        self.note.borrow_mut().take();
        self.confirm.borrow_mut().take();
        self.devices.borrow_mut().refresh();
        app.open_devices_view(DevicesView::new("Devices".to_string()));
        if let Some(view) = app.devices_view_mut() {
            view.focus = DevicesFocus::Rows;
        }
    }

    /// Route a key to the open devices view.
    pub(super) fn key(&self, app: &mut AppModel, key: KeyEvent, incognito: bool) -> DevicesKey {
        let Some(view) = app.devices_view_mut() else {
            return DevicesKey::Handled;
        };
        if !view.controls_visible() && key.code != KeyCode::Esc {
            return DevicesKey::Handled;
        }
        if let Some(confirm) = view.confirm.as_mut() {
            match key.code {
                KeyCode::Esc => {
                    self.confirm.borrow_mut().take();
                }
                KeyCode::Tab | KeyCode::Down => confirm.move_action(true),
                KeyCode::BackTab | KeyCode::Up => confirm.move_action(false),
                KeyCode::Enter => match confirm.selected_action() {
                    Some(CONFIRM) => self.submit(),
                    Some(CANCEL | BACK) | None => {
                        self.confirm.borrow_mut().take();
                    }
                    Some(_) => {}
                },
                _ => {}
            }
            self.render(app, Instant::now(), incognito);
            return DevicesKey::Handled;
        }
        match (key.code, view.focus) {
            (KeyCode::Esc, _) => {
                app.close_devices_view();
                return DevicesKey::Handled;
            }
            (KeyCode::PageDown, _) => view.page_down(),
            (KeyCode::PageUp, _) => view.page_up(),
            (KeyCode::Up | KeyCode::Down, DevicesFocus::Rows) => {
                view.move_row(key.code == KeyCode::Down);
                *self.selected.borrow_mut() = view.selected_id().map(str::to_string);
                self.lost.set(false);
            }
            (KeyCode::Tab | KeyCode::Enter, DevicesFocus::Rows) => {
                view.focus = DevicesFocus::Actions
            }
            (KeyCode::BackTab, DevicesFocus::Actions) if view.action == 0 => {
                view.focus = DevicesFocus::Rows;
            }
            (KeyCode::Tab | KeyCode::Down, DevicesFocus::Actions) => view.move_action(true),
            (KeyCode::BackTab | KeyCode::Up, DevicesFocus::Actions) => view.move_action(false),
            (KeyCode::Enter, DevicesFocus::Actions) => {
                let action = view.selected_action().map(str::to_string);
                match action.as_deref() {
                    // Unavailable says why when chosen.
                    Some(REVOKE | REVOKE_UNAVAILABLE) => self.open_confirm(),
                    Some(PAIR) => {
                        if incognito {
                            let refusal = crate::incognito::incognito_refusal(
                                &localpilot_slash::SlashAction::Remote(
                                    localpilot_slash::RemoteAction::Pair { observe: false },
                                ),
                            )
                            .unwrap_or_else(|| "not available in incognito".to_string());
                            *self.note.borrow_mut() = Some(refusal);
                        } else {
                            app.close_devices_view();
                            return DevicesKey::Pair;
                        }
                    }
                    Some(REFRESH) => {
                        self.note.borrow_mut().take();
                        self.devices.borrow_mut().refresh();
                    }
                    Some(CLOSE) => {
                        app.close_devices_view();
                        return DevicesKey::Handled;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        self.render(app, Instant::now(), incognito);
        DevicesKey::Handled
    }

    /// Open a confirmation for the selected device, if it can be revoked
    /// from the list as it stands now.
    fn open_confirm(&self) {
        self.result.borrow_mut().take();
        let devices = self.devices.borrow();
        let why = if devices.revoke_op().is_some() {
            Some("A revoke is still in progress.")
        } else if !devices.current() {
            Some("The device list is not current. Refresh it first.")
        } else {
            None
        };
        if let Some(why) = why {
            *self.note.borrow_mut() = Some(why.to_string());
            return;
        }
        let selected = self.selected.borrow().clone();
        let row = selected.and_then(|id| current_row(&devices, &id));
        match row {
            None => *self.note.borrow_mut() = Some("Select a device first.".to_string()),
            Some(row) if row.grant == DeviceGrant::Revoked => {
                *self.note.borrow_mut() = Some("This device is already revoked.".to_string());
            }
            Some(row) if !row.actionable => {
                *self.note.borrow_mut() = Some(
                    "This device's record is damaged; revoke it with `localpilot devices revoke`."
                        .to_string(),
                );
            }
            Some(row) => {
                self.note.borrow_mut().take();
                *self.confirm.borrow_mut() = Some(row);
            }
        }
    }

    /// Revoke the confirmation's device (by its frozen full id).
    fn submit(&self) {
        let Some(row) = self.confirm.borrow().clone() else {
            return;
        };
        let refused = self.devices.borrow_mut().revoke(&row).err();
        *self.note.borrow_mut() = refused.map(|refused| {
            match refused {
                Refused::InFlight | Refused::Untold => "A revoke is still in progress.",
                Refused::NotCurrent => {
                    "Reading the device list again first; try again in a moment."
                }
                Refused::NotRevocable => "This device cannot be revoked from here.",
                Refused::Unavailable => "The device registry cannot be reached from here.",
            }
            .to_string()
        });
        if refused.is_none() {
            self.result.borrow_mut().take();
            self.watch.borrow_mut().take();
        }
    }

    /// Every tick: close the view if the session changed or another view
    /// replaced it, take the worker's answers, tell a revoke's answer once,
    /// and keep the view current. `attached` is this session's confirmed
    /// attendance (`None` when not known).
    pub(super) fn tick(
        &self,
        app: &mut AppModel,
        session: Option<SessionId>,
        attached: Option<&[String]>,
        incognito: bool,
    ) {
        let now = Instant::now();
        *self.attached.borrow_mut() = attached.map(<[String]>::to_vec);
        if app.devices_view().is_some() && (self.session.get() != session || app.pairing_replaced())
        {
            app.close_devices_view();
            self.confirm.borrow_mut().take();
        }
        self.devices.borrow_mut().poll(now);
        self.deliver(app);
        if app.devices_view().is_some() {
            self.render(app, now, incognito);
        }
    }

    /// Tell the revoke's state once: on screen (the view shows it) or as a
    /// notice. An answer told is settled; a different state is told anew.
    fn deliver(&self, app: &mut AppModel) {
        let (op, state, row, in_flight) = {
            let devices = self.devices.borrow();
            let Some(revoke) = devices.revoke_op() else {
                return;
            };
            (
                revoke.op,
                revoke.state.clone(),
                revoke.row.clone(),
                revoke.in_flight(),
            )
        };
        if state == RevokeState::Pending {
            return;
        }
        let text = result_text(&state, &row, in_flight);
        let told = Told {
            op,
            state: state.clone(),
            in_flight,
        };
        // Every state of a revoke is told once as a notice, whatever the
        // view shows (it may be off screen, too small, or about to go); the
        // view also keeps the latest.
        if self.told.borrow().as_ref() != Some(&told) {
            match &state {
                RevokeState::Done(
                    Ok(RevokeOutcome::Revoked(_)) | Ok(RevokeOutcome::AlreadyRevoked(_)),
                ) => app.apply_runtime(RuntimeUpdate::Notice(text.clone())),
                _ => app.apply_runtime(RuntimeUpdate::Warning(text.clone())),
            }
            *self.told.borrow_mut() = Some(told);
            *self.result.borrow_mut() = Some(text);
            if let RevokeState::Done(Ok(RevokeOutcome::Revoked(revoked))) = &state {
                *self.watch.borrow_mut() =
                    Some((revoked.id.clone(), revoked.name.clone(), Instant::now()));
            }
        }
        // Answered and told: the slot is free (the result line stays).
        let answered = self
            .devices
            .borrow()
            .revoke_op()
            .is_some_and(|op| !op.in_flight());
        if answered {
            self.devices.borrow_mut().settle(op);
            // A confirmation stays for an explicit retry only after Busy or
            // a failure; any other answer closes it.
            if !matches!(state, RevokeState::Busy | RevokeState::Done(Err(_))) {
                self.confirm.borrow_mut().take();
            }
        }
    }

    /// Put the current state into the view.
    fn render(&self, app: &mut AppModel, now: Instant, incognito: bool) {
        let attached_now = self.attached.borrow().clone();
        let attached = attached_now.as_deref();
        let devices = self.devices.borrow();
        let mut status = Vec::new();
        let listed: Option<&crate::server_devices::DeviceRows> = match devices.listing() {
            Listing::Loading => {
                status.push(if devices.reading_late(now) {
                    "Still reading the device list...".to_string()
                } else {
                    "Loading devices...".to_string()
                });
                None
            }
            Listing::Failed(reason) => {
                status.push(format!("Could not read devices. {reason}"));
                None
            }
            Listing::Ready { rows, stale } => {
                if *stale && devices.reading() {
                    status.push("Reading the device list again...".to_string());
                } else if *stale {
                    status
                        .push("Device list unavailable. Showing last checked values.".to_string());
                }
                Some(rows)
            }
        };
        if let Some(result) = self.result.borrow().as_ref() {
            status.push(result.clone());
        }
        if let Some((id, name, at)) = self.watch.borrow().as_ref() {
            status.push(here_after_revoke(id, name, *at, now, attached));
        }
        if let Some(note) = self.note.borrow().as_ref() {
            status.push(note.clone());
        }
        let rows: Vec<DeviceRow> = listed.map(|listed| listed.rows.clone()).unwrap_or_default();
        if let Some(listed) = listed {
            if listed.rows.is_empty() {
                status.push("No paired devices. Pair a device to get started.".to_string());
            } else if listed.total > listed.rows.len() {
                status.push(format!(
                    "Showing {} of {} devices.",
                    listed.rows.len(),
                    listed.total
                ));
            } else {
                status.push(format!("{} paired device(s).", listed.total));
            }
        }
        // The selection is kept by id; a device that left the list drops it.
        let mut selected = self.selected.borrow_mut();
        if let Some(id) = selected.clone() {
            if listed.is_some() && !rows.iter().any(|row| row.id == id) {
                *selected = None;
                self.lost.set(true);
                if self
                    .confirm
                    .borrow()
                    .as_ref()
                    .is_some_and(|row| row.id == id)
                    && devices.revoke_op().is_none()
                {
                    self.confirm.borrow_mut().take();
                }
            }
        }
        if self.lost.get() {
            status.push("Device no longer listed. Select a device to continue.".to_string());
        } else if selected.is_none() && !rows.is_empty() {
            // Only the first time: never a replacement for a device that left.
            *selected = Some(rows[0].id.clone());
        }
        let width = short_id_width(&rows);
        let lines: Vec<DevicesRow> = rows
            .iter()
            .map(|row| DevicesRow {
                id: row.id.clone(),
                name: row.name.clone(),
                info: row_info(row, width, attached),
                details: details(row),
            })
            .collect();
        let at = selected
            .as_ref()
            .and_then(|id| rows.iter().position(|row| row.id == *id));
        // Revoke is offered only when it would be allowed.
        let revocable = devices.revoke_op().is_none()
            && devices.current()
            && at.is_some_and(|at| rows[at].actionable && rows[at].grant != DeviceGrant::Revoked);
        let confirm = self.confirm.borrow().clone().map(|row| {
            let in_flight = devices
                .revoke_op()
                .is_some_and(|op| op.in_flight() && op.row.id == row.id);
            confirm_view(
                &row,
                attached,
                incognito,
                in_flight,
                self.result.borrow().as_deref(),
                self.note.borrow().as_deref(),
            )
        });
        drop(selected);
        drop(devices);
        let Some(view) = app.devices_view_mut() else {
            return;
        };
        view.status = status;
        view.rows = lines;
        view.selected = at;
        view.set_actions(vec![
            if revocable {
                REVOKE
            } else {
                REVOKE_UNAVAILABLE
            }
            .to_string(),
            PAIR.to_string(),
            REFRESH.to_string(),
            CLOSE.to_string(),
        ]);
        // A confirmation keeps its own selection while it stays open.
        view.confirm = match (view.confirm.take(), confirm) {
            (Some(mut old), Some(new)) if old.id == new.id && old.actions == new.actions => {
                old.lines = new.lines;
                Some(old)
            }
            (_, new) => new,
        };
    }
}

/// The selected device as the current list has it.
fn current_row(devices: &Devices, id: &str) -> Option<DeviceRow> {
    match devices.listing() {
        Listing::Ready { rows, stale: false } => rows.rows.iter().find(|row| row.id == id).cloned(),
        _ => None,
    }
}

fn scope_text(row: &DeviceRow) -> &'static str {
    match row.scope {
        localpilot_rpc::Scope::Control => "control",
        localpilot_rpc::Scope::Observe => "observe",
    }
}

fn reach_text(row: &DeviceRow) -> String {
    match &row.reach {
        DeviceReach::All => "every workspace this server serves".to_string(),
        DeviceReach::Roots { shown, more: 0 } => shown.join(", "),
        DeviceReach::Roots { shown, more } => format!("{}, and {more} more", shown.join(", ")),
    }
}

/// Whether the device is attached to this session, as confirmed telemetry
/// says (`None` when it is not known).
fn here(row: &DeviceRow, attached: Option<&[String]>) -> &'static str {
    match attached {
        None => "unknown here",
        Some(ids) if ids.iter().any(|id| *id == row.id) => "attached here",
        Some(_) if row.grant == DeviceGrant::Active => "detached",
        Some(_) => "not attached",
    }
}

/// What tells a device apart and where it stands (never cut when drawn).
fn row_info(row: &DeviceRow, width: usize, attached: Option<&[String]>) -> String {
    let short: String = row.id.chars().take(width).collect();
    let grant = match row.grant {
        DeviceGrant::Active => "active",
        DeviceGrant::Revoked => "revoked",
        DeviceGrant::NotGrantedHere => "not granted here",
    };
    format!(
        "[{short}]  {}  {grant}  {}",
        scope_text(row),
        here(row, attached)
    )
}

fn details(row: &DeviceRow) -> Vec<String> {
    let mut lines = vec![
        format!("Id: {}", row.id),
        format!("Reach: {}", reach_text(row)),
        format!("Paired: {}", utc(row.created_unix)),
    ];
    if let Some(revoked) = row.revoked_unix {
        lines.push(format!("Revoked: {}", utc(revoked)));
    }
    lines
}

/// A Unix time as `YYYY-MM-DD HH:MM UTC`.
fn utc(unix: u64) -> String {
    let days = i64::try_from(unix / 86_400).unwrap_or(i64::MAX);
    let seconds = unix % 86_400;
    // Days since 1970-01-01 to a civil date (proleptic Gregorian).
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

/// The shortest id prefix (at least 4) that tells every listed device apart.
fn short_id_width(rows: &[DeviceRow]) -> usize {
    let longest = rows
        .iter()
        .map(|row| row.id.chars().count())
        .max()
        .unwrap_or(0);
    (4..=longest.max(4))
        .find(|width| {
            let mut seen = std::collections::HashSet::new();
            rows.iter()
                .all(|row| seen.insert(row.id.chars().take(*width).collect::<String>()))
        })
        .unwrap_or(longest)
}

/// What a revoke came to, as the view and its notice say it.
fn result_text(state: &RevokeState, row: &DeviceRow, in_flight: bool) -> String {
    let name = &row.name;
    match state {
        RevokeState::Pending => format!("Revoking {name}..."),
        RevokeState::Unknown if in_flight => {
            format!("Revoke of {name}: result unknown. Waiting for the registry...")
        }
        RevokeState::Unknown => {
            format!("Revoke of {name}: result unknown. Refresh before trying again.")
        }
        RevokeState::Busy => {
            format!("Could not revoke {name}: the device registry is busy. Try again.")
        }
        RevokeState::Done(Err(reason)) => format!("Could not revoke {name}. {reason}"),
        RevokeState::Done(Ok(RevokeOutcome::Revoked(_))) => {
            format!("Revoked {name}. Running work continues. Access removed in every workspace.")
        }
        RevokeState::Done(Ok(RevokeOutcome::AlreadyRevoked(_))) => {
            "This device was already revoked.".to_string()
        }
        RevokeState::Done(Ok(RevokeOutcome::NotFound)) => {
            "Device not found. Refresh the list.".to_string()
        }
    }
}

/// What this session can see of a revoked device: only its own attendance.
fn here_after_revoke(
    id: &str,
    name: &str,
    at: Instant,
    now: Instant,
    attached: Option<&[String]>,
) -> String {
    match attached {
        None => format!("{name}: revoked; connection here unknown."),
        Some(ids) if ids.iter().any(|attached| attached == id) => {
            if now.duration_since(at) >= DISCONNECT_SEEN {
                format!("{name}: revoked; disconnect not confirmed here.")
            } else {
                format!("{name}: revoked; disconnecting here...")
            }
        }
        Some(_) => format!("{name}: revoked; disconnected here."),
    }
}

fn confirm_view(
    row: &DeviceRow,
    attached: Option<&[String]>,
    incognito: bool,
    in_flight: bool,
    result: Option<&str>,
    note: Option<&str>,
) -> DevicesConfirm {
    let mut lines = vec![
        format!("{} [{}]", row.name, row.id),
        format!("{} | {}", scope_text(row), reach_text(row)),
    ];
    if attached.is_some_and(|ids| ids.iter().any(|id| *id == row.id)) {
        lines.push("Attached here: this session.".to_string());
    }
    lines.push("Removes this device's access in every workspace.".to_string());
    lines.push("It will disconnect. Running work continues.".to_string());
    lines.push("The device must pair again to reconnect.".to_string());
    if incognito {
        lines.push(
            "This is saved even in incognito: the revocation and a metadata-only audit entry (no session text or credentials)."
                .to_string(),
        );
    }
    if in_flight {
        lines.push(format!("Revoking {}...", row.name));
    }
    if let Some(result) = result {
        lines.push(result.to_string());
    }
    if let Some(note) = note {
        lines.push(note.to_string());
    }
    let actions = if in_flight {
        vec![BACK.to_string()]
    } else {
        vec![CANCEL.to_string(), CONFIRM.to_string()]
    };
    DevicesConfirm {
        id: row.id.clone(),
        lines,
        actions,
        selected: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::utc;

    #[test]
    fn times_are_plain_utc() {
        assert_eq!(utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc(951_782_400), "2000-02-29 00:00 UTC");
        assert_eq!(utc(1_793_793_900), "2026-11-04 12:05 UTC");
    }
}
